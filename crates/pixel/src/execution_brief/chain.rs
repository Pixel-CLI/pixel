// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt-start evidence brief: a bounded chain of Pixel lookups the
//! product runs itself, rendered as one compact `[PIXEL:BRIEF]` block.
//!
//! The fixed prefix is `search-content` on the first anchor, `find-code`
//! when that found no file, `find-symbol` for a uid, and `impact` on that
//! uid when the prompt asks about a change or its callers. The
//! [`QuestionKind`] the prompt routes to then spends the ops that are left
//! on the evidence shape the question asked: `evaluate`/`trace` witness
//! hops for a flow, `uses` for covering tests, `list-signatures` and JSON
//! admission for a config question, a facts-freshness probe plus read-only
//! `history` for a rationale, `pack-context` on the picked definition for a
//! bugfix, and `targets_facts` for a feature. Everything shares one
//! deadline and at most [`MAX_OPS`] operations — the deadline, not the
//! count, is the invariant — runs on its own thread, and renders whatever
//! it has when the deadline passes, so it never holds the prompt back. The
//! lookups sit behind [`Evidence`]: `evidence.rs` is the live source, tests
//! bring their own.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use super::intent::Verdict;
use super::routes::QuestionKind;
use super::{SOURCE_EXTENSIONS, Signal, code_signal, names_code};

/// Opening line of the block; a host and a test find the brief by it.
pub(crate) const BRIEF_TAG: &str = "[PIXEL:BRIEF]";
/// Settings key of the opt-out (`brief: false` in `.pixel/config.yaml`).
pub(crate) const BRIEF_FEATURE: &str = "brief";
/// Environment opt-out: `0`, `false` or `off` silences the brief.
pub(crate) const BRIEF_ENV: &str = "PIXEL_BRIEF";
/// One window shared by every operation of one brief, measured from the
/// moment the hook receives the prompt.
pub(crate) const BRIEF_WINDOW: Duration = Duration::from_millis(750);
/// Operations one brief may start, whatever they answer. A kind route may
/// use the headroom (search, concept, symbols, impact, context, one
/// kind op); the shared deadline, not this count, is the invariant.
pub(crate) const MAX_OPS: usize = 6;
/// Rendered size cap; lists give way before a line is cut.
pub(crate) const BRIEF_BYTES: usize = 12288;
/// Match rows one text search pulls before its files are grouped.
pub(crate) const SEARCH_ROWS: usize = 200;
/// Matches one concept search keeps.
pub(crate) const CONCEPT_ROWS: usize = 8;
/// Symbols one name lookup reads.
pub(crate) const SYMBOL_ROWS: u32 = 50;

const MAX_ANCHORS: usize = 4;
const MAX_ANCHOR_CHARS: usize = 80;
/// Shortest snake_case or camelCase word taken for an anchor.
const MIN_CASED_CHARS: usize = 5;
const MAX_FILES: usize = 8;
const MAX_DEFINED: usize = 3;
const MAX_CALLERS: usize = 10;
/// Files a feature brief asks `targets_facts` for and shows; the request
/// bound is the render bound so no fetched row is silently dropped.
pub(crate) const MAX_TARGETS: usize = 4;
/// Longest concept phrase: all the significant words the prompt carries,
/// bounded by characters rather than a word count.
const MAX_CONCEPT_CHARS: usize = 200;
const MIN_CONCEPT_WORD_CHARS: usize = 4;
const MAX_ITEM_CHARS: usize = 120;
/// Chars the `defined` line gives the packed body of the picked
/// definition: wide enough for a few source lines, bounded so it cannot
/// own the whole block.
const DEF_BODY_CHARS: usize = 480;
/// How many of the top files carry an excerpt, and how many lines each.
const MAX_EXCERPT_FILES: usize = 3;
const EXCERPT_LINES: usize = 80;
/// One excerpt's cap in the render — three of these plus the lists stay
/// inside [`BRIEF_BYTES`].
const EXCERPT_CHARS: usize = 2400;
/// Chars of the typed prompt kept for `targets_facts` and follow-ups.
const MAX_TYPED_CHARS: usize = 600;
/// History rows a rationale question pulls.
const HISTORY_ROWS: usize = 3;
/// Token budget of a `pack-context` op.
const CONTEXT_BUDGET_TOKENS: usize = 400;

/// Stems of the words that ask about a change or about who depends on a
/// symbol (`callers`, `depends`, `deprecated` all contain their stem).
const CHANGE_STEMS: &[&str] = &[
    "rename", "remove", "delete", "deprecat", "impact", "caller", "depend", "refactor", "affect",
    "break", "blast",
];
/// Words too common to say what a concept search is about.
const CONCEPT_STOPWORDS: &[&str] = &[
    "about", "could", "does", "each", "every", "from", "have", "into", "make", "please", "should",
    "that", "their", "then", "there", "these", "this", "those", "want", "what", "when", "where",
    "which", "will", "with", "would",
];
/// Extensions of files nobody answers a code question from.
const GENERATED_EXTENSIONS: &[&str] = &["json", "lock"];
/// Directories that hold build output or vendored code.
const GENERATED_DIRS: &[&str] = &["output", "dist", "node_modules"];
const FOOTER: &str = "Answer from this evidence; open a file only if it contradicts you. 0 hits or 0 callers: verify with rg before concluding.";

static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([`'"])([\w./:-]+)(?:\(\))?([`'"])"#)
        .expect("the quoted-anchor pattern is a literal")
});
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\w.-]+(?:/[\w.-]+)+").expect("the path-anchor pattern is a literal")
});
static CASED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-z]+[A-Z]\w*\b|\b[A-Z][a-z0-9]+[A-Z]\w*\b|\b\w+(?:::\w+)+\b|\b\w+_\w+\b")
        .expect("the cased-anchor pattern is a literal")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]+").expect("the word pattern is a literal"));

/// A file an anchor or a concept shows up in, with its first matching line.
/// Kept field-for-field compatible with the hook's own literals; the text
/// a search row carried lives on [`RichHit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileHit {
    pub(crate) path: String,
    pub(crate) line: u64,
}

/// The answer of a file search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) hits: Vec<FileHit>,
    /// The search stopped at its row cap: the list is a prefix.
    pub(crate) capped: bool,
}

/// A file hit whose source carried the matched text itself — the line the
/// pattern hit, or the concept's own words — so the `files` line can show
/// why each file is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RichHit {
    pub(crate) path: String,
    pub(crate) line: u64,
    /// The matched text the source carried, when it carried any.
    pub(crate) text: Option<String>,
}

impl From<FileHit> for RichHit {
    fn from(hit: FileHit) -> Self {
        Self {
            path: hit.path,
            line: hit.line,
            text: None,
        }
    }
}

/// The answer of a file search that keeps the matched text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RichFound {
    pub(crate) hits: Vec<RichHit>,
    /// The search stopped at its row cap: the list is a prefix.
    pub(crate) capped: bool,
}

impl From<Found> for RichFound {
    fn from(found: Found) -> Self {
        Self {
            hits: found.hits.into_iter().map(RichHit::from).collect(),
            capped: found.capped,
        }
    }
}

impl From<RichFound> for Found {
    fn from(found: RichFound) -> Self {
        Self {
            hits: found
                .hits
                .into_iter()
                .map(|hit| FileHit {
                    path: hit.path,
                    line: hit.line,
                })
                .collect(),
            capped: found.capped,
        }
    }
}

/// The answer of a call-path question between two anchors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    /// An ordered chain of names the witness edges traverse, plus cap or
    /// advisory strings the op carried verbatim.
    Path {
        hops: Vec<String>,
        notes: Vec<String>,
    },
    /// The stored relation holds no path — an exhaustive negative answer,
    /// not a gap in the evidence.
    Absent,
}

/// The cheap probe behind a rationale route: whether a facts index can be
/// asked at all. It is a connectivity check, not an index operation, so it
/// sits outside the op budget like `line_at` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StatusProbe {
    /// `facts.fresh` the status op reported; `None` when no route could
    /// say (local route, or a daemon without facts visibility).
    pub(crate) facts_fresh: Option<bool>,
}

/// One history row: the commit's short sha and its subject line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryHit {
    pub(crate) sha: String,
    pub(crate) subject: String,
}

/// One declaration `find-symbol` reports; `uid` is `path#name#kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SymbolHit {
    pub(crate) uid: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) path: String,
    pub(crate) start_line: u64,
    pub(crate) end_line: u64,
}

/// A direct caller `impact` reports: where it lives and which symbol calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallerHit {
    pub(crate) path: String,
    pub(crate) via: String,
    pub(crate) line: u64,
}

/// The lookups of the chain. Every method answers within `deadline` or
/// fails; none of them builds, refreshes or starts anything.
pub(crate) trait Evidence {
    /// Files containing `anchor` as literal text.
    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String>;
    /// The same literal search, keeping each row's matched text when the
    /// source carried it. The default wraps [`Evidence::files_with`], the
    /// textless contract every source already has.
    fn files_matching(&self, anchor: &str, deadline: Instant) -> Result<RichFound, String> {
        self.files_with(anchor, deadline).map(RichFound::from)
    }
    /// Files a free-text concept points at.
    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String>;
    /// The same concept search, keeping the matched text (`raw`/`detail`)
    /// the resolve answer carried.
    fn concept_matching(&self, phrase: &str, deadline: Instant) -> Result<RichFound, String> {
        self.concept(phrase, deadline).map(RichFound::from)
    }
    /// Declarations named `name`.
    fn symbols(&self, name: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String>;
    /// Direct callers (impact depth 1) of a uid, or of a bare name.
    fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String>;
    /// The bounded body of `hit`'s declaration (`pack-context`), plus the
    /// cap or advisory strings the op carried verbatim. An `Err` only
    /// means the body stays absent — the one-line `line_at` read still
    /// stands in.
    fn context(
        &self,
        hit: &SymbolHit,
        budget_tokens: usize,
        _deadline: Instant,
    ) -> Result<(String, Vec<String>), String> {
        let _ = (hit, budget_tokens);
        Err("pack-context unsupported".to_string())
    }
    /// Test files among the direct callers of `uid`, plus the cap or
    /// advisory strings the callers op carried.
    fn test_files(
        &self,
        uid: &str,
        _deadline: Instant,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let _ = uid;
        Err("caller tests unsupported".to_string())
    }
    /// A call path between `from` and `to` (uids or names), bounded to
    /// `budget_ms` on the wire — `evaluate` with a `trace` fallback on the
    /// daemon route, the local graph's own bounded BFS on the other.
    fn flow(
        &self,
        from: &str,
        to: &str,
        budget_ms: u64,
        _deadline: Instant,
    ) -> Result<Flow, String> {
        let _ = (from, to, budget_ms);
        Err("flow unsupported".to_string())
    }
    /// The declarations of `file` (`list-signatures`).
    fn skeleton(&self, file: &str, _deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        let _ = file;
        Err("list-signatures unsupported".to_string())
    }
    /// The connectivity probe of a rationale route: whether a facts index
    /// can be asked at all. Not an index operation — it is free of the op
    /// budget like `line_at` is.
    fn status(&self, _deadline: Instant) -> Result<StatusProbe, String> {
        Err("status unsupported".to_string())
    }
    /// `sha`/`subject` history rows for `phrase`, strictly read-only: a
    /// route that cannot promise that must `Err` rather than write.
    fn history(
        &self,
        phrase: &str,
        limit: usize,
        _deadline: Instant,
    ) -> Result<(Vec<HistoryHit>, Vec<String>), String> {
        let _ = (phrase, limit);
        Err("history unsupported".to_string())
    }
    /// Prompt-start task facts (`targets_facts`): files the stored task
    /// model names, or an `Err` when the route cannot serve it.
    fn task_facts(&self, task: &str, _deadline: Instant) -> Result<Vec<String>, String> {
        let _ = task;
        Err("task facts unsupported".to_string())
    }
    /// The follow-up command for a warm semantic index — model and vectors
    /// already on disk — never an embed or a download. `None` when cold.
    fn semantic_hint(&self, _phrase: &str) -> Option<String> {
        None
    }
    /// One source line at `path:line` — "what it is" for a declaration
    /// (`export const CustomMenu = defineMultiStyleConfig(...)`, not just a
    /// file name). A plain line read, not an index operation; fakes that do
    /// not model source may leave it unsupported.
    fn line_at(&self, path: &str, line: u64, _deadline: Instant) -> Result<String, String> {
        let _ = (path, line);
        Err("source read unsupported".to_string())
    }
    /// The first `max_lines` of `path` — the smallest read that tells the
    /// model what the file does without it opening the file. A plain read,
    /// not an index operation; the deadline bounds it like `line_at`.
    fn file_excerpt(
        &self,
        path: &str,
        max_lines: usize,
        _deadline: Instant,
    ) -> Result<String, String> {
        let _ = (path, max_lines);
        Err("file excerpt unsupported".to_string())
    }
}

/// Where the brief is allowed to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Gate {
    pub(crate) enabled: bool,
    pub(crate) indexed: bool,
}

impl Gate {
    pub(crate) fn read(root: &Path) -> Self {
        Self {
            enabled: crate::config_cmd::feature_enabled(Some(root), BRIEF_FEATURE, BRIEF_ENV),
            indexed: root
                .join(pixel_index::index::SHARD_DIR)
                .join(pixel_index::index::SHARD_FILE)
                .is_file(),
        }
    }

    const fn open(self) -> bool {
        self.enabled && self.indexed
    }
}

/// What the prompt names, at most [`MAX_ANCHORS`] tokens, identifiers first
/// and paths after, in the order the prompt gave them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Anchors(Vec<String>);

impl Anchors {
    /// Anchors of the text the user typed: quoted or backticked identifiers
    /// and paths, then paths with a source extension, then camelCase,
    /// snake_case and `a::b` words.
    pub(crate) fn from_text(typed: &str) -> Self {
        let mut found: Vec<String> = Vec::new();
        for caps in QUOTED.captures_iter(typed) {
            let token = tidy(&caps[2]);
            let backticked = &caps[1] == "`";
            if caps[1] == caps[3]
                && token.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
                && token.chars().any(char::is_alphabetic)
                && (backticked || names_code(token))
            {
                found.push(token.to_string());
            }
        }
        for token in PATHLIKE.find_iter(typed).map(|m| tidy(m.as_str())) {
            if has_source_extension(token) {
                found.push(token.to_string());
            }
        }
        for token in CASED.find_iter(typed).map(|m| m.as_str()) {
            if token.chars().count() >= MIN_CASED_CHARS {
                found.push(token.to_string());
            }
        }
        let mut kept: Vec<String> = Vec::with_capacity(MAX_ANCHORS);
        for token in found {
            if token.chars().count() <= MAX_ANCHOR_CHARS && !kept.contains(&token) {
                kept.push(token);
            }
            if kept.len() == MAX_ANCHORS {
                break;
            }
        }
        // A search takes an identifier; a path is context for the uid pick.
        kept.sort_by_key(|anchor| is_path(anchor));
        Self(kept)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .map(String::as_str)
            .filter(|anchor| !is_path(anchor))
    }

    pub(crate) fn paths(&self) -> Vec<&str> {
        self.0
            .iter()
            .map(String::as_str)
            .filter(|anchor| is_path(anchor))
            .collect()
    }

    /// The literal a text search looks for: the first identifier, else the
    /// stem of the first path.
    fn search_term(&self) -> Option<String> {
        self.names()
            .next()
            .map(ToString::to_string)
            .or_else(|| self.paths().first().map(|path| file_stem(path)))
    }

    /// The name `find-symbol` looks up: the last segment of a `a::b` word.
    fn symbol_name(&self) -> Option<String> {
        self.names()
            .next()
            .map(|name| name.rsplit("::").next().unwrap_or(name).to_string())
            .or_else(|| self.paths().first().map(|path| file_stem(path)))
    }

    /// The lowercase words the anchors consist of: `build`, `decisions`
    /// and `request` inside `build_decisions_request`, `config`, `app`
    /// and `json` inside `config/app.json`. A word that is part of the
    /// target's own name describes what is asked about, not what is asked.
    pub(crate) fn segment_words(&self) -> HashSet<String> {
        self.0
            .iter()
            .flat_map(|anchor| anchor.split(|ch: char| !ch.is_alphanumeric()))
            .map(str::to_lowercase)
            .filter(|word| !word.is_empty())
            .collect()
    }
}

/// Everything the chain needs from the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    anchors: Anchors,
    change_intent: bool,
    /// The routed evidence shape: a verdict label when the judge answered
    /// with one that names a kind, else the typed-text heuristic.
    kind: QuestionKind,
    /// The typed prompt, bounded — `targets_facts`' task and the text a
    /// `next:` suggestion quotes.
    typed: String,
    concept: Option<String>,
}

impl Plan {
    /// `None` for a prompt that does not ask about code, pasted text aside, and
    /// for a continuation or a harness envelope, which are not the user's task.
    /// Tests plan a chain directly; the hook splits this into `code_signal`
    /// plus `from_typed` so a verdict can sit between them.
    #[cfg(test)]
    pub(crate) fn from_prompt(prompt: &str) -> Option<Self> {
        if crate::prompt_continuation::is_trivial_continuation(prompt) {
            return None;
        }
        let (typed, _) = code_signal(prompt)?;
        Some(Self::from_typed(&typed, has_change_intent(&typed), None))
    }

    /// The plan of typed text already judged about-code: `change_intent`
    /// comes from a verdict when one answered, else from the stem table, and
    /// the question kind routes from a verdict label that names an evidence
    /// shape before the heuristic does.
    fn from_typed(typed: &str, change_intent: bool, verdict_label: Option<&str>) -> Self {
        let anchors = Anchors::from_text(typed);
        let kind = verdict_label
            .and_then(QuestionKind::of_verdict)
            .unwrap_or_else(|| QuestionKind::heuristic(typed, &anchors));
        Self {
            anchors,
            // A bugfix prompt wants the symbol's blast radius even when the
            // phrasing held no change stem.
            change_intent: change_intent || kind == QuestionKind::Bugfix,
            kind,
            typed: typed.chars().take(MAX_TYPED_CHARS).collect(),
            concept: concept_phrase(typed),
        }
    }
}

/// The prompt asks about a change or about who depends on something.
pub(crate) fn has_change_intent(typed: &str) -> bool {
    let lower = typed.to_lowercase();
    CHANGE_STEMS.iter().any(|stem| lower.contains(stem))
}

/// The significant words of the prompt that say what a concept search is
/// about: deduplicated, bounded to [`MAX_CONCEPT_CHARS`] at a word edge.
/// The resolver tokenizes the phrase itself, so stopwords and short words
/// are still dropped here — left in, they would weaken the AND-intersection
/// its tiers start with.
fn concept_phrase(typed: &str) -> Option<String> {
    let mut phrase = String::new();
    let mut seen: Vec<String> = Vec::new();
    for word in WORD
        .find_iter(typed)
        .map(|word| word.as_str().to_lowercase())
    {
        if word.chars().count() < MIN_CONCEPT_WORD_CHARS
            || CONCEPT_STOPWORDS.contains(&word.as_str())
            || seen.contains(&word)
        {
            continue;
        }
        if !phrase.is_empty() && phrase.len() + 1 + word.len() > MAX_CONCEPT_CHARS {
            break;
        }
        if !phrase.is_empty() {
            phrase.push(' ');
        }
        phrase.push_str(&word);
        seen.push(word);
    }
    (!phrase.is_empty()).then_some(phrase)
}

/// Strip what surrounds a path or identifier in prose: a leading `./` and a
/// closing sentence mark.
fn tidy(token: &str) -> &str {
    token
        .trim_end_matches(['.', ',', ':', ';'])
        .trim_start_matches("./")
}

fn is_path(anchor: &str) -> bool {
    anchor.contains('/')
}

fn has_source_extension(token: &str) -> bool {
    token
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, ext)| {
            !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        })
}

fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

/// A path whose hits are build output or data, not code a question is about.
pub(crate) fn is_generated(path: &str) -> bool {
    let path = Path::new(path);
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            GENERATED_EXTENSIONS
                .iter()
                .any(|generated| ext.eq_ignore_ascii_case(generated))
        })
        || path.components().any(|part| {
            part.as_os_str()
                .to_str()
                .is_some_and(|name| GENERATED_DIRS.contains(&name))
        })
}

/// The one generated extension a config question still wants: `settings.json`
/// is exactly what such a question asks about.
fn is_json(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// The declaration to ask `impact` about, and why a bare name is used when
/// none can be chosen. A path anchor selects the declaration in that file; a
/// single candidate is taken; several without a path anchor take the first
/// and say so; several that a path anchor does not separate are not guessed.
pub(crate) fn pick_uid<'a>(
    hits: &'a [SymbolHit],
    name: &str,
    path_anchors: &[&str],
) -> (Option<&'a SymbolHit>, Option<String>) {
    let in_anchor = |hit: &SymbolHit| {
        path_anchors.iter().any(|anchor| {
            Path::new(&hit.path).ends_with(anchor) || Path::new(anchor).ends_with(&hit.path)
        })
    };
    if let Some(hit) = hits.iter().find(|hit| in_anchor(hit)) {
        return (Some(hit), None);
    }
    match hits {
        [] => (
            None,
            Some(format!("find-symbol {name}: no uid, bare name used")),
        ),
        [only] => (Some(only), None),
        _ if !path_anchors.is_empty() => (
            None,
            Some(format!(
                "find-symbol {name}: {} candidates, none in the named file, bare name used",
                hits.len()
            )),
        ),
        [first, ..] => (
            Some(first),
            Some(format!(
                "find-symbol {name}: {} candidates, took first",
                hits.len()
            )),
        ),
    }
}

/// A code excerpt read from a file the search found: the path plus enough
/// opening lines to judge what the file does — the difference between a
/// brief that names evidence and one the model can answer from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) path: String,
    pub(crate) content: String,
}

/// What the chain has learned, shared between the worker and the hook.
#[derive(Clone, Debug, Default)]
pub(crate) struct Brief {
    /// Serialize for --json output (transient ops fields omitted).
    anchors: Vec<String>,
    /// The intent a warm local verdict decided, when one drove the plan.
    intent: Option<String>,
    /// The question kind the plan routed to, rendered as `kind:`.
    kind: Option<QuestionKind>,
    files: Vec<RichHit>,
    defined: Vec<SymbolHit>,
    /// First source line of the picked definition, when it was read.
    def_head: Option<String>,
    /// The packed body of the picked definition (`pack-context`), preferred
    /// over `def_head` in the `defined` line.
    def_body: Option<String>,
    /// `path:line — source` for the most likely definition site when no
    /// symbol resolved (`export const` bindings the graph does not index).
    likely_def: Option<String>,
    callers: Vec<CallerHit>,
    /// The ordered flow line of a flow question: `a -> b -> c`, or the
    /// honest negative.
    flow: Option<String>,
    /// Test files a `uses` route found calling the picked symbol.
    tests: Vec<String>,
    /// Declarations a `list-signatures` op read of `skeleton_file`.
    skeleton: Vec<SymbolHit>,
    skeleton_file: Option<String>,
    /// Files a `targets_facts` op named for the task.
    targets: Vec<String>,
    /// History rows (sha + subject) of a rationale route.
    history: Vec<HistoryHit>,
    /// The semantic follow-up command when the index is already warm.
    semantic: Option<String>,
    /// Code excerpts from the top files the search found — the evidence a
    /// model answers from instead of re-reading each file itself.
    excerpts: Vec<Excerpt>,
    /// Cap and advisory strings the ops carried verbatim.
    caps: Vec<String>,
    /// The best follow-up `pixel` command for a partial packet, set by the
    /// routed kind.
    next: Option<String>,
    /// The symbol lookup resolved to exactly one uid: `pack-context` can
    /// follow without a disambiguation round-trip.
    unambiguous_def: bool,
    excluded: Vec<String>,
    unresolved: Vec<String>,
    ops: usize,
    answered: usize,
    searched: bool,
    impacted: bool,
    /// The `uses` route ran and answered (possibly with no test file).
    tested: bool,
    /// The `targets_facts` op ran and answered.
    targeted: bool,
    cut: bool,
    finished: bool,
}

impl Brief {
    /// Fold search hits into the brief: one entry per (path, line) so
    /// distinct same-file sites — the production line and the test line —
    /// both survive. `admit_json` (config questions only) lets `.json`
    /// paths past the generated filter; a credential-shaped path never
    /// enters a brief either way.
    fn absorb(&mut self, found: RichFound, admit_json: bool) {
        for hit in found.hits {
            let json = is_json(&hit.path);
            if admit_json && json && pixel_index::index::credential_path(Path::new(&hit.path)) {
                continue;
            }
            if is_generated(&hit.path) && !(admit_json && json) {
                if !self.excluded.contains(&hit.path) {
                    self.excluded.push(hit.path);
                }
            } else if !self
                .files
                .iter()
                .any(|file| file.path == hit.path && file.line == hit.line)
            {
                self.files.push(hit);
            }
        }
    }
}

fn edit<T>(state: &Mutex<Brief>, change: impl FnOnce(&mut Brief) -> T) -> T {
    change(&mut state.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Count one operation, or refuse it: the cap and the deadline both end the
/// chain, and a refusal marks the brief as cut short.
fn spend(state: &Mutex<Brief>, deadline: Instant) -> bool {
    edit(state, |brief| {
        if brief.ops >= MAX_OPS || Instant::now() >= deadline {
            brief.cut = true;
            false
        } else {
            brief.ops += 1;
            true
        }
    })
}

/// Run the chain against `evidence`, writing into `state` after each
/// operation so a reader that stops waiting still sees what is done.
pub(crate) fn run(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let admit_json = plan.kind == QuestionKind::Config;
    edit(state, |brief| {
        brief.anchors.clone_from(&plan.anchors.0);
        brief.kind = Some(plan.kind);
        // The fallback follow-up of a partial packet; each routed kind
        // overwrites it with its own command.
        brief.next = plan
            .concept
            .clone()
            .map(|phrase| format!("pixel find-code {}", super::routes::q(&phrase)));
    });
    if let Some(term) = plan.anchors.search_term()
        && spend(state, deadline)
    {
        match evidence.files_matching(&term, deadline) {
            Ok(found) => edit(state, |brief| {
                brief.answered += 1;
                brief.searched = true;
                let capped = found.capped;
                brief.absorb(found, admit_json);
                if capped {
                    brief.unresolved.push(format!(
                        "search stopped at {SEARCH_ROWS} rows, the file list is a prefix"
                    ));
                }
            }),
            Err(reason) => edit(state, |brief| {
                brief.unresolved.push(format!("search {term}: {reason}"));
            }),
        }
    }
    if let Some(phrase) = &plan.concept
        && edit(state, |brief| brief.files.is_empty())
        && spend(state, deadline)
    {
        match evidence.concept_matching(phrase, deadline) {
            Ok(found) => edit(state, |brief| {
                brief.answered += 1;
                brief.searched = true;
                let capped = found.capped;
                brief.absorb(found, admit_json);
                if capped {
                    brief.unresolved.push(
                        "find-code: bounded scans capped, the file list is a lower bound"
                            .to_string(),
                    );
                }
            }),
            Err(reason) => edit(state, |brief| {
                brief.unresolved.push(format!("find-code: {reason}"));
            }),
        }
    }
    if let Some(name) = plan.anchors.symbol_name()
        && spend(state, deadline)
    {
        match evidence.symbols(&name, deadline) {
            Ok(hits) => {
                let (pick, note) = pick_uid(&hits, &name, &plan.anchors.paths());
                let target = pick.map_or_else(|| name.clone(), |hit| hit.uid.clone());
                let single = hits.len() == 1 && pick.is_some();
                edit(state, |brief| {
                    brief.answered += 1;
                    brief.defined = ordered(&hits, pick);
                    brief.unambiguous_def = single;
                    brief.unresolved.extend(note);
                });
                if plan.change_intent && spend(state, deadline) {
                    match evidence.callers(&target, deadline) {
                        Ok(callers) => edit(state, |brief| {
                            brief.answered += 1;
                            brief.impacted = true;
                            brief.callers = callers;
                        }),
                        Err(reason) => edit(state, |brief| {
                            brief.unresolved.push(format!("impact {name}: {reason}"));
                        }),
                    }
                }
            }
            Err(reason) => edit(state, |brief| {
                brief
                    .unresolved
                    .push(format!("find-symbol {name}: {reason}"));
            }),
        }
    }
    // Evidence the model answers from: the top files' opening lines.
    // Plain reads, not index ops — each is a stat plus a bounded read, so
    // the deadline covers them without touching the op budget.
    if edit(state, |brief| !brief.files.is_empty()) {
        let paths: Vec<String> = edit(state, |brief| {
            brief
                .files
                .iter()
                // Hidden directories (.zcode/, .git/) hold drafts and
                // metadata, not the source an excerpt should carry.
                .filter(|hit| !hit.path.split('/').any(|part| part.starts_with('.')))
                .take(MAX_EXCERPT_FILES)
                .map(|hit| hit.path.clone())
                .collect()
        });
        for path in paths {
            if Instant::now() >= deadline {
                edit(state, |brief| brief.cut = true);
                break;
            }
            if let Ok(content) = evidence.file_excerpt(&path, EXCERPT_LINES, deadline) {
                edit(state, |brief| {
                    brief.excerpts.push(Excerpt { path, content });
                });
            }
        }
    }
    // The routed kind spends the ops the prefix left on the evidence shape
    // the question asked for.
    run_kind(plan, evidence, state, deadline);
    // "what it is" for the picked definition: one bounded line read, free of
    // the index-operation budget. A kind route that already packed the body
    // makes this redundant; a failure only means the line stays absent.
    let target = edit(state, |brief| {
        (brief.def_body.is_none())
            .then(|| brief.defined.first().cloned())
            .flatten()
    });
    if let Some(hit) = target
        && let Ok(head) = evidence.line_at(&hit.path, hit.start_line, deadline)
    {
        edit(state, |brief| brief.def_head = Some(head));
    }
    // The graph may not resolve a binding (`export const X = f(...)`); when no
    // symbol resolved, the file hit whose stem matches the anchor is the most
    // likely definition site — quote its line so the answer names what it is.
    if edit(state, |brief| brief.defined.is_empty()) {
        let candidate = edit(state, |brief| {
            plan.anchors.symbol_name().and_then(|name| {
                brief
                    .files
                    .iter()
                    .find(|hit| {
                        Path::new(&hit.path)
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .is_some_and(|stem| stem.eq_ignore_ascii_case(&name))
                    })
                    .map(|hit| (hit.path.clone(), hit.line))
            })
        });
        if let Some((path, line)) = candidate
            && let Ok(head) = evidence.line_at(&path, line, deadline)
        {
            edit(state, |brief| {
                // A hit with no line of its own reads line 1, so it is named
                // by bare path rather than by a `:0` that points nowhere.
                brief.likely_def = Some(if line == 0 {
                    format!("{path} — {head}")
                } else {
                    format!("{path}:{line} — {head}")
                });
            });
        }
    }
    // The semantic hint is the last word of an empty brief: the literal and
    // concept searches found nothing, but an already-warm index could.
    if edit(state, |brief| {
        brief.files.is_empty() && brief.semantic.is_none()
    }) {
        let phrase = plan.concept.as_deref().unwrap_or(&plan.typed);
        if let Some(hint) = evidence.semantic_hint(phrase) {
            edit(state, |brief| brief.semantic = Some(hint));
        }
    }
    edit(state, |brief| brief.finished = true);
}

/// The extra ops of the routed kind: each spends the shared deadline like
/// the prefix did, and each lands in the brief under its own line.
fn run_kind(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    match plan.kind {
        QuestionKind::Flow => flow_evidence(plan, evidence, state, deadline),
        QuestionKind::Tests => tests_evidence(evidence, state, deadline),
        QuestionKind::Config => config_evidence(plan, evidence, state, deadline),
        QuestionKind::Rationale => rationale_evidence(plan, evidence, state, deadline),
        QuestionKind::Bugfix => {
            edit(state, |brief| {
                brief.next = brief
                    .defined
                    .first()
                    .map(|hit| format!("pixel impact {}", super::routes::q(&hit.uid)));
            });
            def_body(evidence, state, deadline, false);
        }
        QuestionKind::Feature => feature_evidence(plan, evidence, state, deadline),
        QuestionKind::Lookup => lookup_evidence(evidence, state, deadline),
    }
}

/// `evaluate`/`trace` between the first two symbol anchors: witness hops
/// as an ordered `flow:` line, or the honest negative. A one-anchor reach
/// question names its destination in prose or asks who reaches it — the
/// bounded callers answer.
fn flow_evidence(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let mut names = plan.anchors.names();
    let Some(from_name) = names.next().map(str::to_string) else {
        return;
    };
    let from = edit(state, |brief| {
        brief
            .defined
            .first()
            .map_or(from_name, |hit| hit.uid.clone())
    });
    let to = names
        .next()
        .map(str::to_string)
        .or_else(|| flow_destination(plan));
    let Some(to) = to else {
        // Only one endpoint was named: "who calls X" is the bounded flow
        // answer the ops can give — unless the change-intent prefix
        // already asked it of this same target.
        edit(state, |brief| {
            brief.next = Some(format!("pixel who-calls {}", super::routes::q(&from)));
        });
        let already_asked = edit(state, |brief| {
            brief.impacted
                || brief
                    .unresolved
                    .iter()
                    .any(|note| note.starts_with("impact "))
        });
        if already_asked || !spend(state, deadline) {
            return;
        }
        match evidence.callers(&from, deadline) {
            Ok(callers) => edit(state, |brief| {
                brief.answered += 1;
                brief.impacted = true;
                brief.callers = callers;
            }),
            Err(reason) => edit(state, |brief| {
                brief.unresolved.push(format!("who-calls {from}: {reason}"));
            }),
        }
        return;
    };
    edit(state, |brief| {
        brief.next = Some(format!(
            "pixel evaluate path --from {} --to {}",
            super::routes::q(&from),
            super::routes::q(&to)
        ));
    });
    if !spend(state, deadline) {
        return;
    }
    let budget_ms = deadline
        .checked_duration_since(Instant::now())
        .map_or(0, |left| left.as_millis() as u64);
    match evidence.flow(&from, &to, budget_ms, deadline) {
        Ok(Flow::Path { hops, notes }) => edit(state, |brief| {
            brief.answered += 1;
            brief.flow = Some(hops.join(" -> "));
            brief.caps.extend(notes);
        }),
        Ok(Flow::Absent) => edit(state, |brief| {
            brief.answered += 1;
            brief.flow = Some("no call path in the stored snapshot".to_string());
        }),
        Err(reason) => edit(state, |brief| {
            brief
                .unresolved
                .push(format!("evaluate {from} -> {to}: {reason}"));
        }),
    }
}

/// The destination a one-anchor reach question names in prose: the first
/// content word after the last flow word that is not a segment of an
/// anchor — `render` in "how does start_brief reach the render". An
/// ambiguous or absent word resolves at the `evaluate`/`trace` op, which
/// reports the ambiguity rather than guessing.
fn flow_destination(plan: &Plan) -> Option<String> {
    let anchored = plan.anchors.segment_words();
    let mut seen_flow = false;
    for word in plan
        .typed
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::to_lowercase)
    {
        if word.is_empty() {
            continue;
        }
        if super::routes::asks_flow(&word) {
            seen_flow = true;
            continue;
        }
        if !seen_flow {
            continue;
        }
        if word.len() >= 4
            && !anchored.contains(&word)
            && !CONCEPT_STOPWORDS.contains(&word.as_str())
        {
            return Some(word);
        }
    }
    None
}

/// `uses` on the picked uid, kept to the callers that are test files.
fn tests_evidence(evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let target = edit(state, |brief| {
        brief.defined.first().map(|hit| hit.uid.clone())
    });
    let Some(uid) = target else {
        edit(state, |brief| {
            brief
                .unresolved
                .push("tests: no uid resolved for a callers query".to_string());
        });
        return;
    };
    edit(state, |brief| {
        brief.next = Some(format!("pixel who-calls {}", super::routes::q(&uid)));
    });
    if !spend(state, deadline) {
        return;
    }
    match evidence.test_files(&uid, deadline) {
        Ok((files, caps)) => edit(state, |brief| {
            brief.answered += 1;
            brief.tested = true;
            brief.tests = files;
            brief.caps.extend(caps);
        }),
        Err(reason) => edit(state, |brief| {
            brief.unresolved.push(format!("uses {uid}: {reason}"));
        }),
    }
}

/// `list-signatures` on the first path anchor of a config question; the
/// JSON admission itself happened when the search hits landed.
fn config_evidence(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let Some(file) = plan.anchors.paths().first().map(ToString::to_string) else {
        return;
    };
    edit(state, |brief| {
        brief.next = Some(format!("pixel list-signatures {}", super::routes::q(&file)));
    });
    if !spend(state, deadline) {
        return;
    }
    match evidence.skeleton(&file, deadline) {
        Ok(hits) => edit(state, |brief| {
            brief.answered += 1;
            brief.skeleton = hits;
            brief.skeleton_file = Some(file.clone());
        }),
        Err(reason) => edit(state, |brief| {
            brief
                .unresolved
                .push(format!("list-signatures {file}: {reason}"));
        }),
    }
}

/// A facts-freshness probe, then a read-only history search on the phrase.
/// The probe is a connectivity check, not an index operation: like
/// `line_at` it sits outside the op budget. Absent or stale facts spend
/// nothing — a history query behind them would open the store writable.
fn rationale_evidence(
    plan: &Plan,
    evidence: &dyn Evidence,
    state: &Mutex<Brief>,
    deadline: Instant,
) {
    let probe = evidence.status(deadline);
    let fresh = probe
        .as_ref()
        .is_ok_and(|status| status.facts_fresh == Some(true));
    match &probe {
        Ok(_) if fresh => {}
        Ok(_) => edit(state, |brief| {
            brief
                .unresolved
                .push("history probe: facts index absent or not fresh".to_string());
        }),
        Err(reason) => edit(state, |brief| {
            brief.unresolved.push(format!("history probe: {reason}"));
        }),
    }
    if !fresh {
        return;
    }
    let Some(phrase) = plan.concept.clone() else {
        return;
    };
    edit(state, |brief| {
        brief.next = Some(format!(
            "pixel dig-history --phrase {}",
            super::routes::q(&phrase)
        ));
    });
    if !spend(state, deadline) {
        return;
    }
    match evidence.history(&phrase, HISTORY_ROWS, deadline) {
        Ok((hits, caps)) => edit(state, |brief| {
            brief.answered += 1;
            brief.history = hits;
            brief.caps.extend(caps);
        }),
        Err(reason) => edit(state, |brief| {
            brief.unresolved.push(format!("history {phrase}: {reason}"));
        }),
    }
}

/// `targets_facts` on the typed prompt — daemon route only; a route that
/// cannot serve it names that in `unresolved`.
fn feature_evidence(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    edit(state, |brief| {
        brief.next = Some(format!(
            "pixel scope-task {}",
            super::routes::q(&plan.typed)
        ));
    });
    if !spend(state, deadline) {
        return;
    }
    match evidence.task_facts(&plan.typed, deadline) {
        Ok(paths) => edit(state, |brief| {
            brief.answered += 1;
            brief.targeted = true;
            brief.targets = paths;
        }),
        Err(reason) => edit(state, |brief| {
            brief.unresolved.push(format!("targets_facts: {reason}"));
        }),
    }
}

/// A literal lookup: `pack-context` on the picked definition only when it
/// was the single unambiguous hit.
fn lookup_evidence(evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    def_body(evidence, state, deadline, true);
    edit(state, |brief| {
        brief.next = brief
            .defined
            .first()
            .map(|hit| format!("pixel pack-context {}", super::routes::q(&hit.uid)))
            .or_else(|| brief.next.take());
    });
}

/// `pack-context` on the picked definition: its bounded body lands on the
/// `defined` line in place of the one-line head read. `unambiguous_only`
/// skips the op when several candidates made the pick a guess.
fn def_body(
    evidence: &dyn Evidence,
    state: &Mutex<Brief>,
    deadline: Instant,
    unambiguous_only: bool,
) {
    let pick = edit(state, |brief| {
        (!unambiguous_only || brief.unambiguous_def)
            .then(|| brief.defined.first().cloned())
            .flatten()
    });
    let Some(hit) = pick else {
        return;
    };
    if !spend(state, deadline) {
        return;
    }
    match evidence.context(&hit, CONTEXT_BUDGET_TOKENS, deadline) {
        Ok((body, caps)) => edit(state, |brief| {
            brief.answered += 1;
            // A signatures-only pack answered without a body; the free
            // `line_at` read below still gives `defined` its first line.
            if !body.is_empty() {
                brief.def_body = Some(body);
            }
            brief.caps.extend(caps);
        }),
        Err(reason) => edit(state, |brief| {
            brief
                .unresolved
                .push(format!("pack-context {}: {reason}", hit.name));
        }),
    }
}

/// The declarations to show: the picked one first, the rest as found.
fn ordered(hits: &[SymbolHit], pick: Option<&SymbolHit>) -> Vec<SymbolHit> {
    let mut shown: Vec<SymbolHit> = pick.into_iter().cloned().collect();
    shown.extend(
        hits.iter()
            .filter(|hit| pick.is_none_or(|picked| picked.uid != hit.uid))
            .cloned(),
    );
    shown
}

/// A brief on its way: the worker is running, the deadline is fixed.
pub(crate) struct Pending {
    state: Arc<Mutex<Brief>>,
    done: Receiver<()>,
    deadline: Instant,
}

impl Pending {
    /// Wait for the worker until the shared deadline and render what it has.
    /// `None` when no operation answered: a brief of failures says nothing.
    pub(crate) fn finish(self) -> Option<String> {
        let _ = self
            .done
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()));
        let brief = edit(&self.state, |brief| brief.clone());
        render(&brief)
    }

}

/// Start the brief for `prompt` in `root`, or decline: a switched-off brief,
/// a repository without an index and a prompt that asks nothing about code
/// start no thread and run no lookup.
pub(crate) fn start(prompt: &str, root: &Path) -> Option<Pending> {
    let root = root.to_path_buf();
    start_with(
        prompt,
        Gate::read(&root),
        BRIEF_WINDOW,
        move |deadline| super::evidence::open(&root, deadline),
        super::intent::judge,
    )
}

/// [`start`] with its gate, window, evidence source and intent judge given.
/// `judge` runs only on a weakly code-shaped prompt; a denying verdict ends
/// the brief before any lookup, and no verdict means the heuristic plan.
pub(crate) fn start_with<F, J>(
    prompt: &str,
    gate: Gate,
    window: Duration,
    open: F,
    judge: J,
) -> Option<Pending>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
    J: Fn(&str, Instant) -> Option<Verdict> + Send + 'static,
{
    if !gate.open() {
        return None;
    }
    if crate::prompt_continuation::is_trivial_continuation(prompt) {
        return None;
    }
    let (typed, signal) = code_signal(prompt)?;
    let deadline = Instant::now() + window;
    let state = Arc::new(Mutex::new(Brief::default()));
    let (finished, done) = mpsc::channel();
    let worker = Arc::clone(&state);
    std::thread::Builder::new()
        .name("pixel-brief".into())
        .spawn(move || {
            let plan = match signal {
                Signal::Strong => Plan::from_typed(&typed, has_change_intent(&typed), None),
                Signal::Weak => match judge(&typed, deadline) {
                    Some(verdict) if verdict.denies_brief() => {
                        edit(&worker, |brief| brief.finished = true);
                        let _ = finished.send(());
                        return;
                    }
                    Some(verdict) => {
                        let change_intent = verdict.change_intent();
                        let label = verdict.label.clone();
                        edit(&worker, |brief| {
                            brief.intent =
                                Some(format!("{} ({:.2})", verdict.label, verdict.confidence));
                        });
                        Plan::from_typed(&typed, change_intent, Some(&label))
                    }
                    None => Plan::from_typed(&typed, has_change_intent(&typed), None),
                },
            };
            let evidence = open(deadline);
            run(&plan, evidence.as_ref(), &worker, deadline);
            let _ = finished.send(());
        })
        .ok()?;
    Some(Pending {
        state,
        done,
        deadline,
    })
}

/// How many entries of each list a render shows.
#[derive(Clone, Copy)]
struct Shown {
    files: usize,
    defined: usize,
    callers: usize,
    tests: usize,
    skeleton: usize,
    history: usize,
    targets: usize,
    excluded: usize,
    caps: usize,
    unresolved: usize,
    excerpts: usize,
}

impl Shown {
    fn of(brief: &Brief) -> Self {
        Self {
            files: brief.files.len().min(MAX_FILES),
            defined: brief.defined.len().min(MAX_DEFINED),
            callers: brief.callers.len().min(MAX_CALLERS),
            tests: brief.tests.len().min(MAX_FILES),
            skeleton: brief.skeleton.len().min(MAX_FILES),
            history: brief.history.len().min(HISTORY_ROWS),
            targets: brief.targets.len().min(MAX_TARGETS),
            excluded: brief.excluded.len(),
            caps: brief.caps.len(),
            unresolved: brief.unresolved.len(),
            excerpts: brief.excerpts.len().min(MAX_EXCERPT_FILES),
        }
    }

    /// Drop one entry from the longest list (the earlier of equals in the
    /// order excerpts, excluded, files, tests, targets, skeleton, history,
    /// callers, defined, caps, unresolved); `false` when every list is
    /// already empty. Excerpts go first: they are the biggest lines and the
    /// file list still names what an excerpt carried.
    fn shrink(&mut self) -> bool {
        let widest = [
            self.excerpts,
            self.excluded,
            self.files,
            self.tests,
            self.targets,
            self.skeleton,
            self.history,
            self.callers,
            self.defined,
            self.caps,
            self.unresolved,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        if widest == 0 {
            return false;
        }
        for slot in [
            &mut self.excerpts,
            &mut self.excluded,
            &mut self.files,
            &mut self.tests,
            &mut self.targets,
            &mut self.skeleton,
            &mut self.history,
            &mut self.callers,
            &mut self.defined,
            &mut self.caps,
            &mut self.unresolved,
        ] {
            if *slot == widest {
                *slot -= 1;
                return true;
            }
        }
        false
    }
}

/// The block, at most [`BRIEF_BYTES`], or `None` when nothing answered.
/// A list that does not fit loses entries and says how many.
pub(crate) fn render(brief: &Brief) -> Option<String> {
    if brief.answered == 0 {
        return None;
    }
    let mut shown = Shown::of(brief);
    let mut text = render_with(brief, shown);
    while text.len() > BRIEF_BYTES && shown.shrink() {
        text = render_with(brief, shown);
    }
    Some(text)
}

fn render_with(brief: &Brief, shown: Shown) -> String {
    let mut lines = vec![BRIEF_TAG.to_string()];
    if let Some(intent) = &brief.intent {
        lines.push(format!("intent: {}", clean(intent)));
    }
    if let Some(kind) = brief.kind {
        lines.push(format!("kind: {}", kind.as_str()));
    }
    if !brief.anchors.is_empty() {
        let anchors: Vec<String> = brief.anchors.iter().map(|a| clean(a)).collect();
        lines.push(format!("anchors: {}", anchors.join(", ")));
    }
    if let Some(line) = list_line(
        "defined",
        brief.defined.iter().enumerate().map(|(index, hit)| {
            let mut entry = format!(
                "{} {} {}:{}-{}",
                clean(&hit.kind),
                clean(&hit.name),
                clean(&hit.path),
                hit.start_line,
                hit.end_line
            );
            if index == 0
                && let Some(text) = brief.def_body.as_ref().or(brief.def_head.as_ref())
            {
                entry.push_str(" — ");
                entry.push_str(&clean_n(text, DEF_BODY_CHARS));
            }
            entry
        }),
        shown.defined,
        "; ",
        false,
    ) {
        lines.push(line);
    }
    if let Some(likely) = &brief.likely_def {
        lines.push(format!("likely definition: {}", clean(likely)));
    }
    if let Some(line) = list_line(
        &format!(
            "skeleton {}",
            clean(brief.skeleton_file.as_deref().unwrap_or(""))
        ),
        brief.skeleton.iter().map(|hit| {
            format!(
                "{} {}:{}-{}",
                clean(&hit.kind),
                clean(&hit.name),
                hit.start_line,
                hit.end_line
            )
        }),
        shown.skeleton,
        "; ",
        false,
    ) {
        lines.push(line);
    }
    if let Some(line) = list_line(
        "files",
        brief.files.iter().map(|hit| {
            // A file row the search answered without a line number is a
            // bare path: `path:0` reads as a hit on line 0 of the file.
            let site = if hit.line == 0 {
                clean(&hit.path)
            } else {
                format!("{}:{}", clean(&hit.path), hit.line)
            };
            match &hit.text {
                Some(text) => format!("{site} — {}", clean(text)),
                None => site,
            }
        }),
        shown.files,
        if brief.files.iter().any(|hit| hit.text.is_some()) {
            "; "
        } else {
            " "
        },
        brief.searched,
    ) {
        lines.push(line);
    }
    for excerpt in brief.excerpts.iter().take(shown.excerpts) {
        lines.push(format!("--- {} ---", clean(&excerpt.path)));
        lines.push(clean_n(&excerpt.content, EXCERPT_CHARS));
    }
    if let Some(line) = list_line(
        "callers (impact d1)",
        brief.callers.iter().map(|hit| {
            let site = if hit.line == 0 {
                clean(&hit.via)
            } else {
                format!("{}:{}", clean(&hit.via), hit.line)
            };
            format!("{} -> {site}", clean(&hit.path))
        }),
        shown.callers,
        "; ",
        brief.impacted,
    ) {
        lines.push(line);
    }
    if let Some(flow) = &brief.flow {
        lines.push(format!("flow: {}", clean_n(flow, DEF_BODY_CHARS)));
    }
    if let Some(line) = list_line(
        "tests",
        brief.tests.iter().map(|path| clean(path)),
        shown.tests,
        " ",
        brief.tested,
    ) {
        lines.push(line);
    }
    if let Some(line) = list_line(
        "targets",
        brief.targets.iter().map(|path| clean(path)),
        shown.targets,
        " ",
        brief.targeted,
    ) {
        lines.push(line);
    }
    for hit in brief.history.iter().take(shown.history) {
        lines.push(format!(
            "history: {} {}",
            clean(&hit.sha),
            clean(&hit.subject)
        ));
    }
    if let Some(hint) = &brief.semantic {
        lines.push(format!("semantic: available — {}", clean(hint)));
    }
    if let Some(line) = list_line(
        "excluded (generated)",
        brief.excluded.iter().map(|path| clean(path)),
        shown.excluded,
        ", ",
        false,
    ) {
        lines.push(line);
    }
    if let Some(line) = list_line(
        "caps",
        brief.caps.iter().map(|note| clean(note)),
        shown.caps,
        "; ",
        false,
    ) {
        lines.push(line);
    }
    if let Some(line) = list_line(
        "unresolved",
        brief.unresolved.iter().map(|note| clean(note)),
        shown.unresolved,
        "; ",
        false,
    ) {
        lines.push(line);
    }
    let partial = if brief.finished && !brief.cut {
        String::new()
    } else {
        " | partial: budget".to_string()
    };
    lines.push(format!(
        "coverage: {}/{} ops answered{partial}",
        brief.answered, brief.ops
    ));
    // A brief that was cut, timed out, or left gaps is a partial packet:
    // it names its own next step instead of the whole-confidence footer.
    if brief.cut || !brief.finished || !brief.unresolved.is_empty() {
        lines.push("packet partial — open cited regions or run the named op".to_string());
        if let Some(next) = &brief.next {
            lines.push(format!("next: {}", clean_n(next, DEF_BODY_CHARS)));
        }
    } else {
        lines.push(FOOTER.to_string());
    }
    lines.join("\n")
}




/// `label: a b c (+N more)`. An empty list prints `none` when the operation
/// behind it answered, and no line when it never ran.
fn list_line(
    label: &str,
    items: impl Iterator<Item = String>,
    shown: usize,
    separator: &str,
    answered: bool,
) -> Option<String> {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        return answered.then(|| format!("{label}: none"));
    }
    let hidden = items.len() - shown.min(items.len());
    let head = items
        .iter()
        .take(shown)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(separator);
    Some(match (head.is_empty(), hidden) {
        (_, 0) => format!("{label}: {head}"),
        (true, _) => format!("{label}: {hidden} not shown"),
        (false, _) => format!("{label}: {head} (+{hidden} more)"),
    })
}

/// One line, bounded: repository text never breaks the block's shape.
fn clean(text: &str) -> String {
    clean_n(text, MAX_ITEM_CHARS)
}

/// [`clean`] at a different bound: a definition body keeps several source
/// lines where a path would hold only one.
fn clean_n(text: &str, chars: usize) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [&str; 0] = [];
    const SECOND: Duration = Duration::from_secs(1);
    const OPEN: Gate = Gate {
        enabled: true,
        indexed: true,
    };

    fn hit(path: &str, line: u64) -> FileHit {
        FileHit {
            path: path.into(),
            line,
        }
    }

    fn found(hits: Vec<FileHit>) -> Found {
        Found {
            hits,
            capped: false,
        }
    }

    fn rhit(path: &str, line: u64, text: &str) -> RichHit {
        RichHit {
            path: path.into(),
            line,
            text: Some(text.into()),
        }
    }

    fn rhit_no_text(path: &str, line: u64) -> RichHit {
        RichHit {
            path: path.into(),
            line,
            text: None,
        }
    }

    fn rfound(hits: Vec<RichHit>) -> RichFound {
        RichFound {
            hits,
            capped: false,
        }
    }

    fn history(sha: &str, subject: &str) -> HistoryHit {
        HistoryHit {
            sha: sha.into(),
            subject: subject.into(),
        }
    }

    fn symbol(path: &str, name: &str) -> SymbolHit {
        SymbolHit {
            uid: format!("{path}#{name}#function"),
            name: name.into(),
            kind: "function".into(),
            path: path.into(),
            start_line: 3,
            end_line: 9,
        }
    }

    fn caller(path: &str, via: &str, line: u64) -> CallerHit {
        CallerHit {
            path: path.into(),
            via: via.into(),
            line,
        }
    }

    /// The judge of a hook without a warm daemon: never a verdict, so the
    /// heuristic plan runs exactly as before verdicts existed.
    fn no_verdict(_: &str, _: Instant) -> Option<Verdict> {
        None
    }

    /// A canned verdict: `start_with`'s `judge` for one label.
    fn verdict(label: &'static str) -> impl Fn(&str, Instant) -> Option<Verdict> {
        move |_, _| {
            Some(Verdict {
                label: label.to_string(),
                confidence: 0.9,
            })
        }
    }

    fn anchors(text: &str) -> Vec<String> {
        Anchors::from_text(text).0
    }

    #[test]
    fn anchors_should_prefer_backticked_names_then_paths_and_put_paths_last() {
        assert_eq!(
            anchors("`handleError` in packages/ui/handleError.ts is renamed to reportError"),
            ["handleError", "reportError", "packages/ui/handleError.ts"]
        );
    }

    #[test]
    fn anchors_should_take_cased_words_and_ignore_plain_prose() {
        assert_eq!(
            anchors("why does fetchUser call retry_with_backoff and Foo::bar twice"),
            ["fetchUser", "retry_with_backoff", "Foo::bar"]
        );
        assert_eq!(anchors("where is the parser and how does it work"), NONE);
    }

    #[test]
    fn anchors_should_keep_a_quoted_word_only_when_it_looks_like_code() {
        assert_eq!(anchors("the 'render_page' helper"), ["render_page"]);
        assert_eq!(anchors("it's 'fine' to use \"this\""), NONE);
        assert_eq!(anchors("call `run` now"), ["run"]);
    }

    #[test]
    fn anchors_should_drop_call_parentheses_and_sentence_marks() {
        assert_eq!(anchors("rename `load_config()`."), ["load_config"]);
        assert_eq!(anchors("see `run()`"), ["run"]);
        assert_eq!(anchors("see `src/main.rs`."), ["src/main.rs"]);
        assert_eq!(
            anchors("open ./crates/pixel/src/lib.rs."),
            ["crates/pixel/src/lib.rs"]
        );
    }

    #[test]
    fn anchors_should_not_take_prose_slashes_or_extensionless_paths() {
        assert_eq!(anchors("client/server and/or docs/guide"), NONE);
    }

    #[test]
    fn anchors_should_stop_at_four_and_skip_duplicates() {
        assert_eq!(
            anchors("aaaa_bbbb aaaa_bbbb cccc_dddd eeee_ffff gggg_hhhh iiii_jjjj"),
            ["aaaa_bbbb", "cccc_dddd", "eeee_ffff", "gggg_hhhh"]
        );
    }

    #[test]
    fn anchors_should_skip_a_token_longer_than_the_cap() {
        let long = format!("{}_x", "a".repeat(MAX_ANCHOR_CHARS - 1));
        assert_eq!(anchors(&long), NONE);
        let fits = format!("{}_x", "a".repeat(MAX_ANCHOR_CHARS - 2));
        assert_eq!(anchors(&fits), [fits.as_str()]);
    }

    #[test]
    fn anchors_should_ignore_a_cased_word_below_the_minimum_length() {
        assert_eq!(anchors("a_b and c_de and fgH"), NONE);
        assert_eq!(anchors("x_yzw"), ["x_yzw"]);
    }

    #[test]
    fn search_term_and_symbol_name_should_use_the_identifier_then_the_path_stem() {
        let named = Anchors::from_text("rename Foo::bar in src/foo.rs");
        assert_eq!(named.search_term().as_deref(), Some("Foo::bar"));
        assert_eq!(named.symbol_name().as_deref(), Some("bar"));
        let pathed = Anchors::from_text("who uses crates/pixel/src/handler.rs");
        assert_eq!(pathed.search_term().as_deref(), Some("handler"));
        assert_eq!(pathed.symbol_name().as_deref(), Some("handler"));
        assert_eq!(Anchors::default().search_term(), None);
        assert_eq!(Anchors::default().symbol_name(), None);
    }

    #[test]
    fn change_intent_should_follow_the_stems_and_ignore_a_literal_lookup() {
        for prompt in [
            "rename handleError to reportError",
            "who are the callers of load_config",
            "what depends on the parser",
            "is it safe to delete this",
            "this refactor would break clients",
            "blast radius of the change",
            "mark it as Deprecated",
            "what is the impact on tests",
            "does it affect the cache",
            "remove the flag",
        ] {
            assert!(has_change_intent(prompt), "{prompt}");
        }
        for prompt in [
            "where is fetchUser defined",
            "how does the retry logic work",
            "show me the cache module",
        ] {
            assert!(!has_change_intent(prompt), "{prompt}");
        }
    }

    #[test]
    fn plan_should_decline_a_prompt_that_asks_nothing_about_code() {
        assert_eq!(Plan::from_prompt("ok thanks"), None);
        assert_eq!(Plan::from_prompt("commit and push this"), None);
        for envelope in [
            "<task-notification><task-id>x</task-id>fetchUser retry_count failed</task-notification>",
            "<system-reminder>callers of `fetchUser`</system-reminder>",
            "<command-name>/review</command-name> `fetchUser`",
        ] {
            assert_eq!(Plan::from_prompt(envelope), None, "{envelope}");
        }
        assert!(
            Plan::from_prompt("fix the hook: a <task-notification> reaches `fetchUser`").is_some()
        );
        assert_eq!(
            Plan::from_prompt("<pasted_content id=1>fetchUser retry_count</pasted_content> thanks"),
            None
        );
    }

    #[test]
    fn plan_should_carry_anchors_intent_and_concept_words() {
        let plan = Plan::from_prompt("who are the callers of `fetchUser`?").unwrap();
        assert_eq!(plan.anchors.0, ["fetchUser"]);
        assert!(plan.change_intent);
        assert_eq!(plan.concept.as_deref(), Some("callers fetchuser"));
    }

    #[test]
    fn concept_phrase_should_skip_short_common_and_repeated_words_and_bound_the_chars() {
        // Every significant word rides, deduplicated — nothing is clipped
        // to a word count anymore.
        assert_eq!(
            concept_phrase(
                "how does the session cache expire stale entries after logout today cache"
            ),
            Some("session cache expire stale entries after logout today".to_string())
        );
        let long = format!(
            "find the thing {} tail",
            (0..40)
                .map(|n| format!("paddingword{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let phrase = concept_phrase(&long).expect("a long prompt still has a phrase");
        assert!(phrase.len() <= MAX_CONCEPT_CHARS, "{phrase}");
        assert!(!phrase.ends_with(' '), "{phrase}");
        assert!(phrase.starts_with("find"), "{phrase}");
        assert_eq!(concept_phrase("it is a to do"), None);
    }

    #[test]
    fn generated_files_should_be_data_dumps_and_build_output() {
        for path in [
            "package.json",
            "Cargo.lock",
            "DATA.JSON",
            "output/report.ts",
            "apps/web/dist/main.js",
            "node_modules/pkg/index.js",
        ] {
            assert!(is_generated(path), "{path}");
        }
        for path in [
            "src/json.rs",
            "src/outputs.rs",
            "src/dist_calc.rs",
            "crates/jsonlike/lib.rs",
            "src/lockfile.rs",
        ] {
            assert!(!is_generated(path), "{path}");
        }
    }

    #[test]
    fn pick_uid_should_choose_the_declaration_in_the_named_file() {
        let hits = [
            symbol("apps/web/handleError.ts", "handleError"),
            symbol("packages/ui/handleError.ts", "handleError"),
        ];
        let (pick, note) = pick_uid(&hits, "handleError", &["packages/ui/handleError.ts"]);
        assert_eq!(pick, Some(&hits[1]));
        assert_eq!(note, None);
    }

    #[test]
    fn pick_uid_should_take_a_single_candidate_without_a_note() {
        let hits = [symbol("a.ts", "go")];
        assert_eq!(pick_uid(&hits, "go", &[]), (Some(&hits[0]), None));
        assert_eq!(
            pick_uid(&hits, "go", &["elsewhere/b.ts"]),
            (Some(&hits[0]), None)
        );
    }

    #[test]
    fn pick_uid_should_take_the_first_of_several_and_say_so() {
        let hits = [symbol("a.ts", "go"), symbol("b.ts", "go")];
        let (pick, note) = pick_uid(&hits, "go", &[]);
        assert_eq!(pick, Some(&hits[0]));
        assert_eq!(
            note.as_deref(),
            Some("find-symbol go: 2 candidates, took first")
        );
    }

    #[test]
    fn pick_uid_should_not_guess_when_a_path_anchor_separates_nothing() {
        let hits = [symbol("a.ts", "go"), symbol("b.ts", "go")];
        let (pick, note) = pick_uid(&hits, "go", &["lib/elsewhere.ts"]);
        assert_eq!(pick, None);
        assert_eq!(
            note.as_deref(),
            Some("find-symbol go: 2 candidates, none in the named file, bare name used")
        );
    }

    #[test]
    fn pick_uid_should_fall_back_to_the_bare_name_when_nothing_is_declared() {
        let (pick, note) = pick_uid(&[], "go", &["a.ts"]);
        assert_eq!(pick, None);
        assert_eq!(
            note.as_deref(),
            Some("find-symbol go: no uid, bare name used")
        );
    }

    #[test]
    fn pick_uid_should_match_a_path_anchor_by_whole_components_only() {
        let hits = [
            symbol("src/xhandler.ts", "go"),
            symbol("lib/handler.ts", "go"),
        ];
        let (pick, _) = pick_uid(&hits, "go", &["handler.ts"]);
        assert_eq!(pick, Some(&hits[1]));
    }

    /// A scripted source: each operation logs its call, pauses, and returns
    /// its canned answer.
    struct Fake {
        files: Result<Found, String>,
        /// A `files_matching` answer carrying matched text; when `None`
        /// the fake wraps `files` like the trait's default does.
        rows: Option<Result<RichFound, String>>,
        concept: Result<Found, String>,
        concept_rows: Option<Result<RichFound, String>>,
        symbols: Result<Vec<SymbolHit>, String>,
        callers: Result<Vec<CallerHit>, String>,
        context: Result<(String, Vec<String>), String>,
        test_files: Result<(Vec<String>, Vec<String>), String>,
        flow: Result<Flow, String>,
        skeleton: Result<Vec<SymbolHit>, String>,
        status: Result<StatusProbe, String>,
        history: Result<(Vec<HistoryHit>, Vec<String>), String>,
        task_facts: Result<Vec<String>, String>,
        /// The semantic hint the fake advertises, when it models a warm index.
        semantic: Option<String>,
        /// Source line a `line_at` read answers with, when the fake models
        /// source at all.
        source: Option<String>,
        pause: Duration,
        log: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                files: Ok(Found::default()),
                rows: None,
                concept: Ok(Found::default()),
                concept_rows: None,
                symbols: Ok(Vec::new()),
                callers: Ok(Vec::new()),
                context: Err("pack-context unsupported".into()),
                test_files: Err("caller tests unsupported".into()),
                flow: Err("flow unsupported".into()),
                skeleton: Err("list-signatures unsupported".into()),
                status: Err("status unsupported".into()),
                history: Err("history unsupported".into()),
                task_facts: Err("task facts unsupported".into()),
                semantic: None,
                source: None,
                pause: Duration::ZERO,
                log: Mutex::new(Vec::new()),
            }
        }

        fn note(&self, entry: String) {
            std::thread::sleep(self.pause);
            self.log.lock().unwrap().push(entry);
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Evidence for Fake {
        fn files_with(&self, anchor: &str, _: Instant) -> Result<Found, String> {
            self.note(format!("files_with {anchor}"));
            self.files.clone()
        }
        fn files_matching(&self, anchor: &str, _: Instant) -> Result<RichFound, String> {
            self.note(format!("files_with {anchor}"));
            self.rows
                .clone()
                .unwrap_or_else(|| self.files.clone().map(RichFound::from))
        }
        fn concept(&self, phrase: &str, _: Instant) -> Result<Found, String> {
            self.note(format!("concept {phrase}"));
            self.concept.clone()
        }
        fn concept_matching(&self, phrase: &str, _: Instant) -> Result<RichFound, String> {
            self.note(format!("concept {phrase}"));
            self.concept_rows
                .clone()
                .unwrap_or_else(|| self.concept.clone().map(RichFound::from))
        }
        fn symbols(&self, name: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            self.note(format!("symbols {name}"));
            self.symbols.clone()
        }
        fn callers(&self, target: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            self.note(format!("callers {target}"));
            self.callers.clone()
        }
        fn context(
            &self,
            hit: &SymbolHit,
            budget_tokens: usize,
            _: Instant,
        ) -> Result<(String, Vec<String>), String> {
            self.note(format!("context {} budget {budget_tokens}", hit.uid));
            self.context.clone()
        }
        fn test_files(&self, uid: &str, _: Instant) -> Result<(Vec<String>, Vec<String>), String> {
            self.note(format!("test_files {uid}"));
            self.test_files.clone()
        }
        fn flow(&self, from: &str, to: &str, budget_ms: u64, _: Instant) -> Result<Flow, String> {
            self.note(format!("flow {from} -> {to} ({budget_ms}ms)"));
            self.flow.clone()
        }
        fn skeleton(&self, file: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            self.note(format!("skeleton {file}"));
            self.skeleton.clone()
        }
        fn status(&self, _: Instant) -> Result<StatusProbe, String> {
            self.note("status".to_string());
            self.status.clone()
        }
        fn history(
            &self,
            phrase: &str,
            limit: usize,
            _: Instant,
        ) -> Result<(Vec<HistoryHit>, Vec<String>), String> {
            self.note(format!("history {phrase} limit {limit}"));
            self.history.clone()
        }
        fn task_facts(&self, task: &str, _: Instant) -> Result<Vec<String>, String> {
            self.note(format!("task_facts {task}"));
            self.task_facts.clone()
        }
        fn semantic_hint(&self, phrase: &str) -> Option<String> {
            self.note(format!("semantic_hint {phrase}"));
            self.semantic.clone()
        }
        fn line_at(&self, path: &str, line: u64, _: Instant) -> Result<String, String> {
            self.note(format!("line_at {path}:{line}"));
            self.source.clone().ok_or_else(|| "no source".to_string())
        }
    }

    fn chain(prompt: &str, fake: &Fake, window: Duration) -> Brief {
        let plan = Plan::from_prompt(prompt).unwrap();
        let state = Mutex::new(Brief::default());
        run(&plan, fake, &state, Instant::now() + window);
        state.into_inner().unwrap()
    }

    #[test]
    fn likely_def_should_name_a_line_less_hit_by_its_path_alone() {
        let mut fake = Fake::new();
        // No symbol resolves, so the file whose stem matches the anchor is
        // quoted as the likely definition — with no line number of its own.
        fake.files = Ok(found(vec![hit("src/handleError.ts", 0)]));
        fake.symbols = Ok(Vec::new());
        fake.source = Some("export const handleError = (e) => e".to_string());
        let brief = chain("where is `handleError` defined?", &fake, SECOND);
        assert_eq!(
            brief.likely_def.as_deref(),
            Some("src/handleError.ts — export const handleError = (e) => e")
        );
        assert_eq!(fake.calls().last().unwrap(), "line_at src/handleError.ts:0");
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nlikely definition: src/handleError.ts — export const handleError"),
            "{text}"
        );
        assert!(!text.contains(":0"), "{text}");
    }

    #[test]
    fn likely_def_should_keep_the_line_of_a_hit_that_has_one() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/handleError.ts", 7)]));
        fake.symbols = Ok(Vec::new());
        fake.source = Some("export const handleError = (e) => e".to_string());
        let brief = chain("where is `handleError` defined?", &fake, SECOND);
        assert!(
            brief
                .likely_def
                .as_deref()
                .is_some_and(|head| head.starts_with("src/handleError.ts:7 — ")),
            "{:?}",
            brief.likely_def
        );
    }

    #[test]
    fn chain_should_search_resolve_a_uid_and_run_impact_on_it_for_a_change_prompt() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![
            hit("apps/web/page.tsx", 4),
            hit("data/out.json", 1),
        ]));
        fake.symbols = Ok(vec![symbol("packages/ui/handleError.ts", "handleError")]);
        fake.callers = Ok(vec![caller("apps/web/page.tsx", "Page", 12)]);
        fake.context = Ok((
            "export function handleError(e) { return \"boom\"; }".into(),
            Vec::new(),
        ));
        let brief = chain(
            "handleError in packages/ui/handleError.ts is being renamed to reportError",
            &fake,
            SECOND,
        );
        assert_eq!(
            fake.calls(),
            [
                "files_with handleError",
                "symbols handleError",
                "callers packages/ui/handleError.ts#handleError#function",
                "context packages/ui/handleError.ts#handleError#function budget 400"
            ]
        );
        assert_eq!(brief.files, [rhit_no_text("apps/web/page.tsx", 4)]);
        assert_eq!(brief.excluded, ["data/out.json"]);
        assert_eq!(brief.callers, [caller("apps/web/page.tsx", "Page", 12)]);
        assert_eq!(brief.kind, Some(QuestionKind::Lookup));
        assert_eq!(
            brief.def_body.as_deref(),
            Some("export function handleError(e) { return \"boom\"; }")
        );
        assert_eq!((brief.ops, brief.answered), (4, 4));
        assert!(brief.finished && !brief.cut);
    }

    #[test]
    fn chain_should_skip_impact_for_a_literal_lookup() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        fake.symbols = Ok(vec![symbol("src/a.ts", "fetchUser")]);
        fake.context = Ok(("function fetchUser() {}".into(), Vec::new()));
        let brief = chain("where is fetchUser defined", &fake, SECOND);
        assert_eq!(
            fake.calls(),
            [
                "files_with fetchUser",
                "symbols fetchUser",
                "context src/a.ts#fetchUser#function budget 400"
            ]
        );
        assert_eq!(brief.ops, 3);
        assert!(brief.callers.is_empty());
        assert!(!brief.impacted);
        assert_eq!(brief.def_body.as_deref(), Some("function fetchUser() {}"));
    }

    #[test]
    fn lookup_context_should_be_skipped_when_the_pick_is_one_of_several() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        fake.symbols = Ok(vec![
            symbol("src/a.ts", "fetchUser"),
            symbol("src/b.ts", "fetchUser"),
        ]);
        let brief = chain("where is fetchUser defined", &fake, SECOND);
        // Two candidates made the pick a guess: `pack-context` does not
        // follow it, but the one-line `line_at` read still does.
        assert!(!fake.calls().iter().any(|call| call.starts_with("context")));
        assert_eq!(
            brief.unresolved,
            ["find-symbol fetchUser: 2 candidates, took first"]
        );
        assert!(fake.calls().contains(&"line_at src/a.ts:3".to_string()));
    }

    #[test]
    fn chain_should_fall_back_to_the_bare_name_and_record_it() {
        let fake = Fake::new();
        let brief = chain("rename `fetchUser` everywhere", &fake, SECOND);
        assert_eq!(
            fake.calls(),
            [
                "files_with fetchUser",
                "concept rename fetchuser everywhere",
                "symbols fetchUser",
                "callers fetchUser",
                "semantic_hint rename fetchuser everywhere"
            ]
        );
        assert_eq!(
            brief.unresolved,
            ["find-symbol fetchUser: no uid, bare name used"]
        );
        assert_eq!(brief.ops, 4);
        assert!(brief.impacted);
    }

    #[test]
    fn chain_should_search_concepts_only_when_no_file_was_found() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        chain("explain how `fetchUser` retries", &fake, SECOND);
        assert!(!fake.calls().iter().any(|call| call.starts_with("concept")));
        let mut none = Fake::new();
        none.concept = Ok(found(vec![hit("src/retry.ts", 7)]));
        let brief = chain("how does the retry logic back off", &none, SECOND);
        assert_eq!(none.calls(), ["concept retry logic back"]);
        assert_eq!(brief.files, [rhit_no_text("src/retry.ts", 7)]);
    }

    #[test]
    fn chain_should_note_a_failed_operation_and_not_count_it_as_an_answer() {
        let mut fake = Fake::new();
        fake.files = Err("text index is not current".into());
        fake.concept = Err("needs a running daemon".into());
        fake.symbols = Err("graph is stale".into());
        fake.callers = Err("graph is stale".into());
        let brief = chain("callers of `fetchUser`", &fake, SECOND);
        assert_eq!(brief.answered, 0);
        assert_eq!(brief.ops, 4);
        assert_eq!(
            brief.unresolved,
            [
                "search fetchUser: text index is not current",
                "find-code: needs a running daemon",
                "find-symbol fetchUser: graph is stale",
                "who-calls fetchUser: graph is stale"
            ]
        );
        assert_eq!(render(&brief), None);
        // The reach question asks for the endpoint's callers: the op ran
        // even though the symbol lookup failed, and failed like the rest.
        assert!(fake.calls().contains(&"callers fetchUser".to_string()));
    }

    #[test]
    fn chain_should_report_an_ambiguous_impact_as_unresolved() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("a.ts", "go_now"), symbol("b.ts", "go_now")]);
        fake.callers = Err("ambiguous name; re-call with uid".into());
        let brief = chain("who are the callers of go_now", &fake, SECOND);
        assert!(
            fake.calls()
                .contains(&"callers a.ts#go_now#function".to_string())
        );
        assert_eq!(
            brief.unresolved,
            [
                "find-symbol go_now: 2 candidates, took first",
                "impact go_now: ambiguous name; re-call with uid"
            ]
        );
        assert_eq!(brief.defined[0].path, "a.ts");
    }

    #[test]
    fn a_flow_prompt_should_evaluate_between_the_first_two_symbol_anchors() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("a.ts", "render_page")]);
        fake.flow = Ok(Flow::Path {
            hops: vec![
                "render_page".into(),
                "middleware".into(),
                "send_response".into(),
            ],
            notes: vec!["traversal capped at depth 8".into()],
        });
        let brief = chain(
            "how does `render_page` reach `send_response`",
            &fake,
            SECOND,
        );
        assert_eq!(brief.kind, Some(QuestionKind::Flow));
        let call = fake
            .calls()
            .iter()
            .find(|call| call.starts_with("flow "))
            .cloned();
        assert!(
            call.as_deref().is_some_and(|entry| {
                entry.starts_with("flow a.ts#render_page#function -> send_response (")
                    && entry.ends_with("ms)")
            }),
            "{call:?}"
        );
        assert_eq!(
            brief.flow.as_deref(),
            Some("render_page -> middleware -> send_response")
        );
        assert_eq!(brief.caps, ["traversal capped at depth 8"]);
        assert_eq!(
            brief.next.as_deref(),
            Some("pixel evaluate path --from 'a.ts#render_page#function' --to 'send_response'")
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nflow: render_page -> middleware -> send_response\n"),
            "{text}"
        );
        assert!(
            text.contains("\ncaps: traversal capped at depth 8\n"),
            "{text}"
        );
    }

    #[test]
    fn a_flow_without_a_path_should_render_the_honest_negative() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("a.ts", "go_one")]);
        fake.flow = Ok(Flow::Absent);
        let brief = chain("how does `go_one` reach `go_two`", &fake, SECOND);
        assert_eq!(
            brief.flow.as_deref(),
            Some("no call path in the stored snapshot")
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nflow: no call path in the stored snapshot\n"),
            "{text}"
        );
    }

    #[test]
    fn a_flow_op_error_should_land_in_unresolved_and_keep_the_next_step() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("a.ts", "go_one")]);
        fake.flow = Err("no call path within depth 8".into());
        let brief = chain("how does `go_one` reach `go_two`", &fake, SECOND);
        assert_eq!(
            brief.unresolved,
            ["evaluate a.ts#go_one#function -> go_two: no call path within depth 8"]
        );
        let text = render(&brief).unwrap();
        assert!(text.contains("\npacket partial —"), "{text}");
        assert!(
            text.contains(
                "\nnext: pixel evaluate path --from 'a.ts#go_one#function' --to 'go_two'"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_tests_prompt_should_list_the_test_files_calling_the_pick() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("src/retry.ts", "retry_loop")]);
        fake.test_files = Ok((
            vec!["tests/retry_test.ts".into(), "specs/retry_spec.ts".into()],
            vec!["callers capped at 200".into()],
        ));
        let brief = chain("which tests cover `retry_loop`", &fake, SECOND);
        assert_eq!(brief.kind, Some(QuestionKind::Tests));
        assert_eq!(
            fake.calls(),
            [
                "files_with retry_loop",
                "concept tests cover retry loop",
                "symbols retry_loop",
                "test_files src/retry.ts#retry_loop#function",
                "line_at src/retry.ts:3",
                "semantic_hint tests cover retry loop"
            ]
        );
        assert_eq!(brief.tests, ["tests/retry_test.ts", "specs/retry_spec.ts"]);
        assert!(brief.tested);
        assert_eq!(brief.caps, ["callers capped at 200"]);
        assert_eq!(
            brief.next.as_deref(),
            Some("pixel who-calls 'src/retry.ts#retry_loop#function'")
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\ntests: tests/retry_test.ts specs/retry_spec.ts\n"),
            "{text}"
        );
    }

    #[test]
    fn a_tests_prompt_without_a_pick_should_say_why_no_op_ran() {
        let fake = Fake::new();
        let brief = chain("which tests cover `retry_loop`", &fake, SECOND);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|call| call.starts_with("test_files"))
        );
        assert_eq!(
            brief.unresolved,
            [
                "find-symbol retry_loop: no uid, bare name used",
                "tests: no uid resolved for a callers query"
            ]
        );
    }

    #[test]
    fn a_config_prompt_should_admit_json_and_skeleton_the_named_file() {
        let mut fake = Fake::new();
        fake.rows = Some(Ok(rfound(vec![
            rhit("cfg/settings.json", 2, "\"hook\": \"on\""),
            rhit("src/a.ts", 3, "load_config()"),
        ])));
        fake.skeleton = Ok(vec![symbol("cfg/settings.json", "load_config")]);
        let brief = chain(
            "how is the hook configured in cfg/settings.json",
            &fake,
            SECOND,
        );
        assert_eq!(brief.kind, Some(QuestionKind::Config));
        // The JSON hit was admitted past the generated filter; a non-config
        // question would have named it under `excluded`.
        assert_eq!(
            brief.files,
            [
                rhit("cfg/settings.json", 2, "\"hook\": \"on\""),
                rhit("src/a.ts", 3, "load_config()")
            ]
        );
        assert!(brief.excluded.is_empty());
        assert!(
            fake.calls()
                .contains(&"skeleton cfg/settings.json".to_string()),
            "{:?}",
            fake.calls()
        );
        assert_eq!(brief.skeleton_file.as_deref(), Some("cfg/settings.json"));
        let text = render(&brief).unwrap();
        assert!(
            text.contains(
                "\nfiles: cfg/settings.json:2 — \"hook\": \"on\"; src/a.ts:3 — load_config()\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("\nskeleton cfg/settings.json: function load_config:3-9\n"),
            "{text}"
        );
    }

    #[test]
    fn a_rationale_prompt_should_probe_then_run_history_when_facts_are_fresh() {
        let mut fake = Fake::new();
        fake.status = Ok(StatusProbe {
            facts_fresh: Some(true),
        });
        fake.history = Ok((
            vec![
                history("abc12345", "introduce the retry loop"),
                history("def67890", "bound its backoff"),
            ],
            Vec::new(),
        ));
        let brief = chain("when was `retry_loop` introduced", &fake, SECOND);
        assert_eq!(brief.kind, Some(QuestionKind::Rationale));
        let calls = fake.calls();
        let status_at = calls.iter().position(|call| call == "status");
        let history_at = calls.iter().position(|call| call.starts_with("history"));
        assert!(
            status_at.is_some() && history_at.is_some() && status_at < history_at,
            "{calls:?}"
        );
        assert_eq!(
            calls[history_at.unwrap()],
            "history retry loop introduced limit 3"
        );
        assert_eq!(brief.history.len(), 2);
        assert_eq!(
            brief.next.as_deref(),
            Some("pixel dig-history --phrase 'retry loop introduced'")
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nhistory: abc12345 introduce the retry loop\n"),
            "{text}"
        );
        assert!(
            text.contains("\nhistory: def67890 bound its backoff\n"),
            "{text}"
        );
    }

    #[test]
    fn a_rationale_prompt_should_spend_nothing_beyond_the_probe_when_facts_are_stale() {
        for probe in [
            Ok(StatusProbe {
                facts_fresh: Some(false),
            }),
            Ok(StatusProbe { facts_fresh: None }),
            Err("daemon down".into()),
        ] {
            let answered = probe.is_ok();
            let mut fake = Fake::new();
            fake.status = probe.clone();
            let brief = chain("why does the cache expire", &fake, SECOND);
            assert!(
                !fake.calls().iter().any(|call| call.starts_with("history")),
                "{probe:?}"
            );
            let expected = if answered {
                "history probe: facts index absent or not fresh"
            } else {
                "history probe: daemon down"
            };
            assert!(
                brief.unresolved.iter().any(|note| note == expected),
                "{probe:?} {:?}",
                brief.unresolved
            );
        }
    }

    #[test]
    fn a_verdict_label_should_route_the_plan_to_its_kind() {
        let plan = Plan::from_typed("fix the crash", true, Some("bugfix"));
        assert_eq!(plan.kind, QuestionKind::Bugfix);
        for label in ["bugfix", "refactor", "review"] {
            let plan = Plan::from_typed("tidy this", false, Some(label));
            assert_eq!(plan.kind, QuestionKind::Bugfix, "{label}");
            assert!(plan.change_intent, "{label}");
        }
        let plan = Plan::from_typed("add an endpoint", false, Some("feature"));
        assert_eq!(plan.kind, QuestionKind::Feature);
        // investigate/question/ops name no evidence shape: the heuristic
        // on the typed text still decides.
        let plan = Plan::from_typed(
            "how does `go_one` reach `go_two`",
            false,
            Some("investigate"),
        );
        assert_eq!(plan.kind, QuestionKind::Flow);
    }

    #[test]
    fn a_bugfix_prompt_should_run_impact_then_pack_context_on_the_pick() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("src/cache.ts", "expire")]);
        fake.callers = Ok(vec![caller("src/api.ts", "handler", 12)]);
        fake.context = Ok(("fn expire() {\n  ttl = 0;\n}".into(), Vec::new()));
        let brief = chain("fix the crash in `expire`", &fake, SECOND);
        assert_eq!(brief.kind, Some(QuestionKind::Bugfix));
        // The defect word routed the kind; the kind routed the blast
        // radius even without a change stem in the phrasing.
        assert_eq!(
            fake.calls(),
            [
                "files_with expire",
                "concept crash expire",
                "symbols expire",
                "callers src/cache.ts#expire#function",
                "context src/cache.ts#expire#function budget 400",
                "semantic_hint crash expire"
            ]
        );
        assert_eq!(
            brief.def_body.as_deref(),
            Some("fn expire() {\n  ttl = 0;\n}")
        );
        assert_eq!(
            brief.next.as_deref(),
            Some("pixel impact 'src/cache.ts#expire#function'")
        );
        let text = render(&brief).unwrap();
        assert!(text.contains("\nkind: bugfix\n"), "{text}");
        assert!(text.contains("— fn expire() {   ttl = 0; }"), "{text}");
    }

    #[test]
    fn a_verdict_bugfix_should_render_its_kind_and_intent() {
        let fake = Fake::new();
        let pending = start_with(
            "fix the crash in the expire path",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("bugfix"),
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(text.contains("\nkind: bugfix\n"), "{text}");
        assert!(text.contains("\nintent: bugfix (0.90)\n"), "{text}");
    }

    #[test]
    fn a_feature_verdict_should_route_to_task_facts() {
        let mut fake = Fake::new();
        fake.task_facts = Ok(vec!["src/new/route.ts".into(), "src/new/handler.ts".into()]);
        let pending = start_with(
            "add an export endpoint to the report page",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("feature"),
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(text.contains("\nkind: feature\n"), "{text}");
        assert!(
            text.contains("\ntargets: src/new/route.ts src/new/handler.ts\n"),
            "{text}"
        );
        assert!(text.ends_with(FOOTER), "{text}");
    }

    #[test]
    fn a_feature_prompt_should_note_the_route_when_task_facts_is_unavailable() {
        let mut fake = Fake::new();
        fake.task_facts = Err("task facts need a running daemon".into());
        let pending = start_with(
            "add an export endpoint to the report page",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("feature"),
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(
            text.contains("targets_facts: task facts need a running daemon"),
            "{text}"
        );
        assert!(!text.contains("\ntargets:"), "{text}");
        assert!(text.contains("\npacket partial —"), "{text}");
        assert!(
            text.contains("\nnext: pixel scope-task 'add an export endpoint to the report page'"),
            "{text}"
        );
    }

    #[test]
    fn an_investigate_verdict_should_fall_back_to_the_lookup_heuristic() {
        let fake = Fake::new();
        let pending = start_with(
            "how does the login flow work",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("investigate"),
        )
        .unwrap();
        // A label naming no evidence shape falls back to the heuristic:
        // no anchors, no kind words — the plain lookup.
        let text = pending.finish().unwrap();
        assert!(text.contains("\nkind: lookup\n"), "{text}");
        assert!(text.contains("\nfiles: none\n"), "{text}");
    }

    #[test]
    fn the_semantic_hint_should_close_an_empty_brief_when_the_index_is_warm() {
        let mut fake = Fake::new();
        fake.semantic = Some("pixel search-meaning 'retry logic'".into());
        let brief = chain("how does the retry logic back off", &fake, SECOND);
        assert_eq!(
            brief.semantic.as_deref(),
            Some("pixel search-meaning 'retry logic'")
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nsemantic: available — pixel search-meaning 'retry logic'\n"),
            "{text}"
        );
    }

    #[test]
    fn matched_text_should_ride_the_files_line() {
        let mut fake = Fake::new();
        fake.rows = Some(Ok(rfound(vec![
            rhit("src/a.ts", 3, "handleError(e)"),
            rhit("src/a.ts", 9, "return handleError(e)"),
        ])));
        let brief = chain("where is handleError used", &fake, SECOND);
        // Both sites of one file stayed: the production line and the call line.
        assert_eq!(
            brief.files,
            [
                rhit("src/a.ts", 3, "handleError(e)"),
                rhit("src/a.ts", 9, "return handleError(e)")
            ]
        );
        let text = render(&brief).unwrap();
        assert!(
            text.contains(
                "\nfiles: src/a.ts:3 — handleError(e); src/a.ts:9 — return handleError(e)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn spend_should_refuse_after_the_cap_and_after_the_deadline() {
        let state = Mutex::new(Brief::default());
        let later = Instant::now() + SECOND;
        for _ in 0..MAX_OPS {
            assert!(spend(&state, later));
        }
        assert!(!spend(&state, later));
        let brief = edit(&state, |brief| brief.clone());
        assert_eq!((brief.ops, brief.cut), (MAX_OPS, true));

        let fresh = Mutex::new(Brief::default());
        assert!(!spend(&fresh, Instant::now()));
        let brief = edit(&fresh, |brief| brief.clone());
        assert_eq!((brief.ops, brief.cut), (0, true));
    }

    #[test]
    fn chain_should_stop_at_the_deadline_and_mark_the_brief_cut() {
        let mut fake = Fake::new();
        fake.pause = Duration::from_millis(120);
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        let brief = chain("callers of `fetchUser`", &fake, Duration::from_millis(60));
        assert_eq!(fake.calls(), ["files_with fetchUser"]);
        assert!(brief.cut);
        assert_eq!(brief.ops, 1);
        let text = render(&brief).unwrap();
        assert!(
            text.contains("coverage: 1/1 ops answered | partial: budget"),
            "{text}"
        );
        assert!(
            text.contains("packet partial — open cited regions or run the named op"),
            "{text}"
        );
    }

    #[test]
    fn pending_should_return_nothing_when_no_operation_answers_before_the_deadline() {
        let mut fake = Fake::new();
        fake.pause = Duration::from_millis(400);
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        let started = Instant::now();
        let pending = start_with(
            "where is `fetchUser` used",
            OPEN,
            Duration::from_millis(150),
            move |_| Box::new(fake),
            no_verdict,
        )
        .unwrap();
        assert_eq!(pending.finish(), None);
        assert!(
            started.elapsed() < Duration::from_millis(390),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pending_should_render_what_answered_before_the_deadline() {
        struct Slow;
        impl Evidence for Slow {
            fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
                Ok(found(vec![hit("src/a.ts", 2)]))
            }
            fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
                Ok(Found::default())
            }
            fn symbols(&self, _: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
                std::thread::sleep(Duration::from_millis(500));
                Ok(Vec::new())
            }
            fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
                Ok(Vec::new())
            }
        }
        let started = Instant::now();
        let pending = start_with(
            "where is `fetchUser` used",
            OPEN,
            Duration::from_millis(200),
            |_| Box::new(Slow),
            no_verdict,
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(480),
            "{:?}",
            started.elapsed()
        );
        assert!(text.starts_with(BRIEF_TAG), "{text}");
        assert!(text.contains("files: src/a.ts:2\n"), "{text}");
        assert!(
            text.contains("coverage: 1/2 ops answered | partial: budget"),
            "{text}"
        );
    }

    #[test]
    fn start_should_decline_when_switched_off_unindexed_or_not_about_code() {
        let open = |_: Instant| -> Box<dyn Evidence> { Box::new(Fake::new()) };
        assert!(start_with("callers of `fetchUser`", OPEN, SECOND, open, no_verdict).is_some());
        for (prompt, gate) in [
            (
                "callers of `fetchUser`",
                Gate {
                    enabled: false,
                    indexed: true,
                },
            ),
            (
                "callers of `fetchUser`",
                Gate {
                    enabled: true,
                    indexed: false,
                },
            ),
            ("thanks, that works", OPEN),
            ("commit this and push", OPEN),
        ] {
            assert!(
                start_with(prompt, gate, SECOND, open, no_verdict).is_none(),
                "{prompt}"
            );
        }
    }

    #[test]
    fn gate_should_read_the_index_from_the_shard_file() {
        let dir = std::env::temp_dir().join(format!("pixel-brief-gate-{}", std::process::id()));
        let shard_dir = dir.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&shard_dir).unwrap();
        assert!(!Gate::read(&dir).indexed);
        std::fs::write(shard_dir.join(pixel_index::index::SHARD_FILE), b"x").unwrap();
        assert!(Gate::read(&dir).indexed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gate_should_open_only_when_enabled_and_indexed() {
        for (enabled, indexed, open) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            assert_eq!(Gate { enabled, indexed }.open(), open);
        }
    }

    fn full_brief() -> Brief {
        Brief {
            anchors: vec!["handleError".into(), "src/handleError.ts".into()],
            intent: Some("bugfix (0.90)".into()),
            kind: Some(QuestionKind::Bugfix),
            files: vec![
                rhit("apps/web/page.tsx", 4, "return handleError(e)"),
                rhit_no_text("apps/web/other.tsx", 9),
            ],
            defined: vec![symbol("src/handleError.ts", "handleError")],
            def_head: Some("export function handleError(e: Error): string {".into()),
            def_body: Some("export function handleError(e) {\n  return \"boom\";\n}".into()),
            likely_def: Some("src/x.ts:1 — export const x".into()),
            callers: vec![caller("apps/web/page.tsx", "Page", 12)],
            flow: Some("one -> two -> three".into()),
            tests: vec!["tests/handleError_test.ts".into()],
            skeleton: vec![symbol("cfg/config.json", "load")],
            skeleton_file: Some("cfg/config.json".into()),
            targets: vec!["src/new.ts".into()],
            history: vec![history("abc1234", "fix retry")],
            semantic: Some("pixel search-meaning 'handle errors'".into()),
            excerpts: vec![],
            caps: vec!["context truncated at budget".into()],
            next: Some("pixel impact 'src/handleError.ts#handleError#function'".into()),
            unambiguous_def: true,
            excluded: vec!["data/out.json".into()],
            unresolved: vec!["find-symbol handleError: 2 candidates, took first".into()],
            ops: 3,
            answered: 3,
            searched: true,
            impacted: true,
            tested: true,
            targeted: true,
            cut: false,
            finished: true,
        }
    }

    #[test]
    fn render_should_lay_out_every_line_in_order() {
        assert_eq!(
            render(&full_brief()).unwrap(),
            [
                "[PIXEL:BRIEF]",
                "intent: bugfix (0.90)",
                "kind: bugfix",
                "anchors: handleError, src/handleError.ts",
                "defined: function handleError src/handleError.ts:3-9 — export function handleError(e) {   return \"boom\"; }",
                "likely definition: src/x.ts:1 — export const x",
                "skeleton cfg/config.json: function load:3-9",
                "files: apps/web/page.tsx:4 — return handleError(e); apps/web/other.tsx:9",
                "callers (impact d1): apps/web/page.tsx -> Page:12",
                "flow: one -> two -> three",
                "tests: tests/handleError_test.ts",
                "targets: src/new.ts",
                "history: abc1234 fix retry",
                "semantic: available — pixel search-meaning 'handle errors'",
                "excluded (generated): data/out.json",
                "caps: context truncated at budget",
                "unresolved: find-symbol handleError: 2 candidates, took first",
                "coverage: 3/3 ops answered",
                "packet partial — open cited regions or run the named op",
                "next: pixel impact 'src/handleError.ts#handleError#function'",
            ]
            .join("\n")
        );
    }

    #[test]
    fn render_should_keep_the_plain_footer_when_nothing_is_missing() {
        let mut brief = Brief {
            anchors: vec!["fetchUser".into()],
            kind: Some(QuestionKind::Lookup),
            files: vec![rhit_no_text("src/a.ts", 2)],
            ops: 2,
            answered: 2,
            searched: true,
            finished: true,
            ..Brief::default()
        };
        let text = render(&brief).unwrap();
        assert!(text.ends_with(FOOTER), "{text}");
        assert!(!text.contains("packet partial"), "{text}");
        assert!(!text.contains("next:"), "{text}");
        brief.cut = true;
        let text = render(&brief).unwrap();
        assert!(text.contains("packet partial"), "{text}");
        assert!(!text.ends_with(FOOTER), "{text}");
    }

    #[test]
    fn render_should_omit_the_line_number_of_a_caller_without_one() {
        let mut brief = full_brief();
        brief.callers = vec![caller("apps/web/page.tsx", "Page", 0)];
        assert!(
            render(&brief)
                .unwrap()
                .contains("\ncallers (impact d1): apps/web/page.tsx -> Page\n")
        );
    }

    #[test]
    fn render_should_omit_the_line_number_of_a_file_without_one() {
        let mut brief = full_brief();
        brief.files = vec![
            rhit_no_text("apps/web/page.tsx", 0),
            rhit_no_text("packages/ui/handleError.ts", 1),
        ];
        let text = render(&brief).unwrap();
        assert!(
            text.contains("\nfiles: apps/web/page.tsx packages/ui/handleError.ts:1\n"),
            "{text}"
        );
        // Every shape a `path:0` could take: the bare hit, and the pair with
        // a real line beside it.
        assert!(!text.contains(":0"), "{text}");
    }

    #[test]
    fn render_should_say_none_for_an_answered_empty_list_and_omit_an_unrun_one() {
        let brief = Brief {
            anchors: vec!["fetchUser".into()],
            ops: 1,
            answered: 1,
            searched: true,
            finished: true,
            ..Brief::default()
        };
        let text = render(&brief).unwrap();
        assert!(text.contains("\nfiles: none\n"), "{text}");
        assert!(!text.contains("callers (impact"), "{text}");
        assert!(!text.contains("defined:"), "{text}");
        assert!(text.contains("coverage: 1/1 ops answered\n"), "{text}");
        let impacted = Brief {
            impacted: true,
            ..brief
        };
        assert!(
            render(&impacted)
                .unwrap()
                .contains("\ncallers (impact d1): none\n")
        );
    }

    #[test]
    fn render_should_return_nothing_when_no_operation_answered() {
        assert_eq!(render(&Brief::default()), None);
        let failed = Brief {
            ops: 2,
            unresolved: vec!["search x: down".into()],
            ..Brief::default()
        };
        assert_eq!(render(&failed), None);
    }

    #[test]
    fn render_should_show_the_list_caps_and_name_what_is_hidden() {
        let mut brief = full_brief();
        brief.files = (0..30)
            .map(|n| rhit_no_text(&format!("apps/web/some/deep/dir/component_{n}.tsx"), n))
            .collect();
        brief.callers = (0..30)
            .map(|n| {
                caller(
                    &format!("apps/web/some/deep/dir/caller_{n}.tsx"),
                    "render",
                    n + 1,
                )
            })
            .collect();
        brief.defined = (0..5)
            .map(|n| symbol(&format!("src/d{n}.ts"), "go"))
            .collect();
        let text = render(&brief).unwrap();
        assert!(text.len() <= BRIEF_BYTES, "{} bytes: {text}", text.len());
        assert!(text.contains("component_7.tsx:7 (+22 more)"), "{text}");
        assert!(
            text.contains("caller_9.tsx -> render:10 (+20 more)"),
            "{text}"
        );
        assert!(text.contains("src/d2.ts:3-9 (+2 more)"), "{text}");
        assert_eq!(text.lines().next(), Some(BRIEF_TAG));
        assert_eq!(
            text.lines().last(),
            Some("next: pixel impact 'src/handleError.ts#handleError#function'")
        );
    }

    #[test]
    fn render_should_drop_whole_entries_until_it_fits_and_stop_as_soon_as_it_does() {
        let mut brief = full_brief();
        brief.files = (0..64)
            .map(|n| rhit_no_text(&format!("{}{n}", "d".repeat(MAX_ITEM_CHARS - 1)), 1))
            .collect();
        brief.callers = (0..80)
            .map(|n| {
                caller(
                    &format!("{}{n}", "c".repeat(MAX_ITEM_CHARS - 1)),
                    &"v".repeat(60),
                    1,
                )
            })
            .collect();
        brief.unresolved = (0..80)
            .map(|n| format!("{}{n}", "u".repeat(MAX_ITEM_CHARS)))
            .collect();
        let text = render(&brief).unwrap();
        assert!(text.len() <= BRIEF_BYTES, "{} bytes", text.len());
        assert!(text.len() > BRIEF_BYTES - 200, "{} bytes", text.len());
        assert!(text.contains("coverage: 3/3 ops answered"), "{text}");
        assert!(text.contains("packet partial"), "{text}");
        for label in ["files:", "callers (impact d1):", "unresolved:"] {
            let line = text.lines().find(|line| line.starts_with(label));
            assert!(
                line.is_some_and(|line| line.contains(" more)")),
                "{label}: {text}"
            );
        }
    }

    #[test]
    fn shrink_should_drop_from_the_longest_list_and_break_ties_in_priority_order() {
        let mut shown = Shown {
            files: 1,
            defined: 1,
            callers: 1,
            tests: 1,
            skeleton: 1,
            history: 1,
            targets: 1,
            excluded: 1,
            caps: 1,
            unresolved: 1,
            excerpts: 0,
        };
        let counts = |shown: &Shown| {
            [
                shown.excluded,
                shown.files,
                shown.tests,
                shown.targets,
                shown.skeleton,
                shown.history,
                shown.callers,
                shown.defined,
                shown.caps,
                shown.unresolved,
            ]
        };
        let mut order = Vec::new();
        let mut prev = counts(&shown);
        while shown.shrink() {
            let now = counts(&shown);
            order.push(
                now.iter()
                    .zip(&prev)
                    .position(|(after, before)| after < before)
                    .expect("shrink drops exactly one entry"),
            );
            prev = now;
        }
        // All ones drop in the stated priority order, one at a time.
        assert_eq!(order, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut uneven = Shown {
            files: 2,
            defined: 1,
            callers: 5,
            tests: 0,
            skeleton: 0,
            history: 0,
            targets: 0,
            excluded: 0,
            caps: 0,
            unresolved: 3,
            excerpts: 0,
        };
        assert!(uneven.shrink());
        assert_eq!((uneven.files, uneven.callers, uneven.unresolved), (2, 4, 3));
    }

    #[test]
    fn list_line_should_cap_and_count_the_hidden_entries() {
        let items = || ["a", "b", "c"].into_iter().map(String::from);
        assert_eq!(
            list_line("x", items(), 3, " ", false).as_deref(),
            Some("x: a b c")
        );
        assert_eq!(
            list_line("x", items(), 2, " ", false).as_deref(),
            Some("x: a b (+1 more)")
        );
        assert_eq!(
            list_line("x", items(), 0, " ", false).as_deref(),
            Some("x: 3 not shown")
        );
        assert_eq!(
            list_line("x", std::iter::empty(), 0, " ", true).as_deref(),
            Some("x: none")
        );
        assert_eq!(list_line("x", std::iter::empty(), 0, " ", false), None);
    }

    #[test]
    fn clean_should_flatten_control_characters_and_bound_the_length() {
        assert_eq!(clean("a\nb\t[PIXEL]\r"), "a b [PIXEL] ");
        assert_eq!(
            clean(&"x".repeat(MAX_ITEM_CHARS + 5)).chars().count(),
            MAX_ITEM_CHARS
        );
        assert_eq!(
            clean(&"é".repeat(MAX_ITEM_CHARS)).chars().count(),
            MAX_ITEM_CHARS
        );
    }

    #[test]
    fn absorb_should_split_generated_files_and_dedupe_by_path_and_line() {
        let mut brief = Brief::default();
        brief.absorb(
            rfound(vec![
                rhit("src/a.ts", 3, "handleError(e)"),
                rhit_no_text("src/a.ts", 9),
                rhit_no_text("src/a.ts", 9),
                rhit_no_text("package.json", 1),
                rhit_no_text("package.json", 4),
            ]),
            false,
        );
        // Distinct same-file sites — the production line and the test line
        // — both survive; only an exact (path, line) repeat is deduped.
        assert_eq!(
            brief.files,
            [
                rhit("src/a.ts", 3, "handleError(e)"),
                rhit_no_text("src/a.ts", 9)
            ]
        );
        assert_eq!(brief.excluded, ["package.json"]);
    }

    #[test]
    fn absorb_should_admit_json_only_for_a_config_question_and_never_a_credential() {
        let mut config = Brief::default();
        config.absorb(
            rfound(vec![
                rhit_no_text("cfg/settings.json", 2),
                rhit_no_text(".aws/credentials.json", 1),
                rhit_no_text("data/out.lock", 1),
            ]),
            true,
        );
        assert_eq!(config.files, [rhit_no_text("cfg/settings.json", 2)]);
        // A credential-shaped path is dropped silently — never even named
        // in `excluded`; a `.lock` stays generated either way.
        assert_eq!(config.excluded, ["data/out.lock"]);
        let mut lookup = Brief::default();
        lookup.absorb(
            rfound(vec![
                rhit_no_text("cfg/settings.json", 2),
                rhit_no_text("src/a.ts", 3),
            ]),
            false,
        );
        assert_eq!(lookup.files, [rhit_no_text("src/a.ts", 3)]);
        assert_eq!(lookup.excluded, ["cfg/settings.json"]);
    }

    #[test]
    fn a_weak_prompt_denied_by_a_verdict_should_finish_with_no_brief() {
        let pending = start_with(
            "how does the login flow work",
            OPEN,
            SECOND,
            |_| Box::new(Fake::new()),
            verdict("none"),
        )
        .expect("a weak prompt still starts a pending brief");
        assert_eq!(pending.finish(), None);
    }

    #[test]
    fn a_weak_prompt_without_a_verdict_should_keep_the_heuristic_plan() {
        let mut fake = Fake::new();
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let pending = start_with(
            "how does the login flow work",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(text.contains("src/login.ts:3"), "{text}");
        assert!(!text.contains("intent:"), "{text}");
    }

    #[test]
    fn a_verdict_should_be_rendered_as_the_plan_intent() {
        let mut fake = Fake::new();
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let pending = start_with(
            "how does the login flow work",
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("investigate"),
        )
        .unwrap();
        let text = pending.finish().unwrap();
        assert!(text.contains("intent: investigate (0.90)"), "{text}");
        assert!(text.contains("kind: lookup"), "{text}");
        assert!(text.contains("src/login.ts:3"), "{text}");
    }

    #[test]
    fn a_strong_prompt_should_never_ask_the_judge() {
        let pending = start_with(
            "where is `fetchUser` used",
            OPEN,
            SECOND,
            |_| Box::new(Fake::new()),
            |_, _| panic!("a strong prompt must not reach the judge"),
        )
        .unwrap();
        let _ = pending.finish();
    }
}
