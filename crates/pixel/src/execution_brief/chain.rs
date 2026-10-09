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
//!
//! A prompt in plain language (no identifier, no code word) starts differently:
//! the intent judge, the relevance probe and the meaning search run at once on
//! the shared deadline, and the relevance decision, with no model in it, says
//! whether the prompt is about this repository at all. Off topic ends the
//! brief at once and renders nothing; on topic fuses the meaning leads with
//! the files the prompt's words meet in, and the routed kind goes on from
//! there. A weakly code-shaped prompt gets the same probes as its retriever and
//! records the decision without enforcing it ([`ENFORCE_GATE_ON_WEAK`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use super::decision_log::{self, Record};
use super::intent::Verdict;
use super::relevance::{self, RelevanceInput};
use super::routes::QuestionKind;
use super::{SOURCE_EXTENSIONS, Signal, brief_signal, names_code};

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
pub(crate) const BRIEF_BYTES: usize = 2048;
/// Size cap of a brief built for a plain-language prompt; its own constant
/// so an experiment can widen it without touching code-shaped briefs.
pub(crate) const PROSE_BRIEF_BYTES: usize = 2048;
/// Whether an off-topic relevance decision silences a weakly code-shaped
/// prompt's brief. The decision is computed and logged either way; the
/// evaluation decides when it starts to bind.
pub(crate) const ENFORCE_GATE_ON_WEAK: bool = false;
/// Leads a meaning search returns: as many files as a brief shows.
const MEANING_LIMIT: usize = MAX_FILES;
/// Rank constant of the reciprocal-rank fusion of the meaning leads and the
/// co-files: large enough that the top of either list outweighs the bottom of
/// both, the value the fusion literature settled on.
const FUSE_K: f64 = 60.0;
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

/// One lead of a natural-language search over code: the best chunk of a file
/// for the question. A lead ranks, it does not prove relevance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MeaningHit {
    pub(crate) path: String,
    /// Inclusive 1-based line range of the chunk.
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    /// The symbol the chunk belongs to, when it belongs to one.
    pub(crate) symbol: Option<String>,
    /// Comparable within one answer, not across questions.
    pub(crate) score: f64,
    /// The head of the chunk on one line.
    pub(crate) snippet: String,
}

/// The answer of a relevance probe: the weights the gate scores (the
/// daemon's, so the formula has one spelling) and the line each co-file was
/// found at.
#[derive(Clone, Debug)]
pub(crate) struct RelevanceAnswer {
    pub(crate) input: RelevanceInput,
    /// The co-files in the input's order, each with the line and text that
    /// showed its keywords.
    pub(crate) lines: Vec<RichHit>,
}

/// The lookups of the chain. Every method answers within `deadline` or
/// fails; none of them builds, refreshes or starts anything. `Sync`: a
/// plain-language prompt asks several of them from threads at once.
pub(crate) trait Evidence: Sync {
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
    /// How widely the words of `typed` occur in the repository and the files
    /// they meet in (`facts.relevance`): what the relevance gate scores. An
    /// `Err` is "cannot tell", which a plain-language prompt treats as off
    /// topic.
    fn relevance(&self, typed: &str, deadline: Instant) -> Result<RelevanceAnswer, String> {
        let _ = (typed, deadline);
        Err("unavailable".to_string())
    }
    /// At most `limit` leads for a natural-language `query`, best first, from
    /// vectors already resident in memory — never an embed or a download.
    fn meaning(
        &self,
        query: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<MeaningHit>, String> {
        let _ = (query, limit, deadline);
        Err("unavailable".to_string())
    }
    /// One source line at `path:line` — "what it is" for a declaration
    /// (`export const CustomMenu = defineMultiStyleConfig(...)`, not just a
    /// file name). A plain line read, not an index operation; fakes that do
    /// not model source may leave it unsupported.
    fn line_at(&self, path: &str, line: u64, _deadline: Instant) -> Result<String, String> {
        let _ = (path, line);
        Err("source read unsupported".to_string())
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
        let (typed, _) = super::code_signal(prompt)?;
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

/// What the gates decided about the prompt: the relevance probe and the
/// intent judge, for the log and for the render.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Admission {
    /// No gate looked at it: a code-shaped prompt, or a weak one whose
    /// probes had nothing to say.
    #[default]
    Unjudged,
    /// The prompt is about this repository.
    Open,
    /// The repository does not talk about the prompt, or could not be asked.
    Closed(String),
    /// The intent judge said the prompt is not a coding task.
    Denied(String),
}

impl Admission {
    /// The decision as the log spells it.
    fn label(&self) -> &'static str {
        match self {
            Self::Unjudged => "unjudged",
            Self::Open => "open",
            Self::Closed(_) => "closed",
            Self::Denied(_) => "denied",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Closed(reason) | Self::Denied(reason) => Some(reason),
            Self::Unjudged | Self::Open => None,
        }
    }
}

/// What the chain has learned, shared between the worker and the hook.
#[derive(Clone, Debug, Default)]
pub(crate) struct Brief {
    /// How the prompt came to be briefed.
    signal: Option<Signal>,
    admission: Admission,
    /// The gate refused the prompt and the refusal binds: nothing renders.
    silenced: bool,
    /// The relevance decision, when the probe answered.
    relevance: Option<relevance::Verdict>,
    /// The intent judge's verdict, when it answered.
    judged: Option<Verdict>,
    /// The `confidence:` line of a brief built on the relevance decision.
    confidence: Option<String>,
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

/// Why a prompt got no brief at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Declined {
    /// `brief: false` in the configuration or `PIXEL_BRIEF=0`.
    Disabled,
    /// The repository has no published index.
    Unindexed,
    /// An acknowledgement, a greeting or a harness envelope.
    Continuation,
    /// Neither code-shaped nor plain language about a task.
    NotAboutCode,
    /// The operating system gave no thread to run it on.
    NoWorker,
}

impl Declined {
    /// The reason as the decision log spells it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unindexed => "unindexed",
            Self::Continuation => "continuation",
            Self::NotAboutCode => "not_about_code",
            Self::NoWorker => "no_worker",
        }
    }
}

/// The record of a prompt that got no brief.
pub(crate) fn declined_record(declined: Declined, prompt: &str) -> Record {
    let typed = super::typed_text(prompt);
    let typed = typed.trim();
    Record {
        ts_ms: pixel_task::now_ms(),
        signal: None,
        gate: "declined",
        enforced: false,
        reason: Some(declined.as_str().to_string()),
        score: None,
        best_file: None,
        features: None,
        judge: None,
        kind: None,
        ops: 0,
        answered: 0,
        bytes: 0,
        elapsed_ms: 0,
        typed: decision_log::logged_typed(typed),
        sha256: decision_log::sha256_hex(typed),
    }
}

/// A brief on its way: the worker is running, the deadline is fixed.
pub(crate) struct Pending {
    state: Arc<Mutex<Brief>>,
    done: Receiver<()>,
    deadline: Instant,
    started: Instant,
    /// Where the decision is recorded, when it is.
    log: Option<PathBuf>,
    /// The text the brief judged: the typed prompt, or its last paragraph.
    task: String,
    signal: Signal,
}

/// A finished brief: the block, when there is one, and what was decided.
pub(crate) struct Finished {
    pub(crate) text: Option<String>,
    pub(crate) record: Record,
}

/// Whether the gate's verdict decided if the brief was shown: always for
/// plain language, for a weak prompt only when [`ENFORCE_GATE_ON_WEAK`] says
/// so or the judge refused it.
const fn gate_enforced(signal: Signal, silenced: bool, enforce_weak: bool) -> bool {
    match signal {
        Signal::Prose => true,
        Signal::Weak => silenced || enforce_weak,
        Signal::Strong => false,
    }
}

impl Pending {
    /// Wait for the worker until the shared deadline and render what it has.
    /// `None` when no operation answered: a brief of failures says nothing.
    pub(crate) fn finish(self) -> Option<String> {
        self.finish_with_record().text
    }

    /// [`Pending::finish`], and the decision it recorded.
    pub(crate) fn finish_with_record(self) -> Finished {
        let _ = self
            .done
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()));
        let brief = edit(&self.state, |brief| brief.clone());
        let text = render(&brief);
        let record = Record {
            ts_ms: pixel_task::now_ms(),
            signal: Some(self.signal.as_str()),
            gate: brief.admission.label(),
            enforced: gate_enforced(self.signal, brief.silenced, ENFORCE_GATE_ON_WEAK),
            reason: brief.admission.reason().map(str::to_string),
            score: brief.relevance.as_ref().map(|verdict| verdict.score),
            best_file: brief
                .relevance
                .as_ref()
                .and_then(|verdict| verdict.best_file.clone()),
            features: brief
                .relevance
                .as_ref()
                .map(|verdict| verdict.features.clone()),
            judge: brief
                .judged
                .as_ref()
                .map(|verdict| (verdict.label.clone(), verdict.confidence)),
            kind: brief.kind.map(QuestionKind::as_str),
            ops: brief.ops,
            answered: brief.answered,
            bytes: text.as_ref().map_or(0, String::len),
            elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            typed: decision_log::logged_typed(&self.task),
            sha256: decision_log::sha256_hex(&self.task),
        };
        if let Some(path) = &self.log {
            // Best effort: a log that cannot be written never costs the prompt.
            let _ = decision_log::append(path, &record.line(), decision_log::MAX_LINES);
        }
        Finished { text, record }
    }
}

/// Start the brief for `prompt` in `root`, or decline: a switched-off brief,
/// a repository without an index and a prompt that asks nothing about code
/// start no thread and run no lookup.
pub(crate) fn start(prompt: &str, root: &Path) -> Option<Pending> {
    try_start(prompt, root).ok()
}

/// [`start`], saying why it declined.
pub(crate) fn try_start(prompt: &str, root: &Path) -> Result<Pending, Declined> {
    let log = decision_log::path_for(root);
    let gate = Gate::read(root);
    let root = root.to_path_buf();
    try_start_with(
        prompt,
        gate,
        BRIEF_WINDOW,
        move |deadline| super::evidence::open(&root, deadline),
        super::intent::judge,
        log,
    )
}

/// [`start`] with its gate, window, evidence source and intent judge given and
/// no decision log: the seam the chain's tests start from.
#[cfg(test)]
pub(crate) fn start_with<F, J>(
    prompt: &str,
    gate: Gate,
    window: Duration,
    open: F,
    judge: J,
) -> Option<Pending>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
    J: Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
{
    try_start_with(prompt, gate, window, open, judge, None).ok()
}

/// [`try_start`] with its gate, window, evidence source, intent judge and
/// decision log given. The judge runs on a weakly code-shaped or plain
/// prompt, beside the relevance probe and the meaning search; a denying
/// verdict ends the brief before any evidence op, and no verdict means the
/// heuristic plan.
pub(crate) fn try_start_with<F, J>(
    prompt: &str,
    gate: Gate,
    window: Duration,
    open: F,
    judge: J,
    log: Option<PathBuf>,
) -> Result<Pending, Declined>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
    J: Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
{
    if !gate.open() {
        return Err(if gate.enabled {
            Declined::Unindexed
        } else {
            Declined::Disabled
        });
    }
    if crate::prompt_continuation::is_trivial_continuation(prompt) {
        return Err(Declined::Continuation);
    }
    let Some((typed, signal)) = brief_signal(prompt) else {
        if let Some(path) = &log {
            let record = declined_record(Declined::NotAboutCode, prompt);
            let _ = decision_log::append(path, &record.line(), decision_log::MAX_LINES);
        }
        return Err(Declined::NotAboutCode);
    };
    let started = Instant::now();
    let deadline = started + window;
    let state = Arc::new(Mutex::new(Brief {
        signal: Some(signal),
        ..Brief::default()
    }));
    let (finished, done) = mpsc::channel();
    let worker = Arc::clone(&state);
    let task = typed.clone();
    std::thread::Builder::new()
        .name("pixel-brief".into())
        .spawn(move || {
            work(&typed, signal, deadline, &worker, &finished, open, &judge);
            let _ = finished.send(());
        })
        .map_err(|_| Declined::NoWorker)?;
    Ok(Pending {
        state,
        done,
        deadline,
        started,
        log,
        task,
        signal,
    })
}

/// The worker of one brief: plan, ask, run.
fn work<F, J>(
    typed: &str,
    signal: Signal,
    deadline: Instant,
    state: &Mutex<Brief>,
    finished: &Sender<()>,
    open: F,
    judge: &J,
) where
    F: FnOnce(Instant) -> Box<dyn Evidence>,
    J: Fn(&str, Instant) -> Option<Verdict> + Sync,
{
    if signal == Signal::Strong {
        let plan = Plan::from_typed(typed, has_change_intent(typed), None);
        let evidence = open(deadline);
        run(&plan, evidence.as_ref(), state, deadline);
        return;
    }
    let evidence = open(deadline);
    let mut settled = |got: &Gathered| match refusal(signal, got, false, ENFORCE_GATE_ON_WEAK) {
        Some(admission) => {
            close(state, got, admission);
            let _ = finished.send(());
            true
        }
        None => false,
    };
    let got = gather(
        typed,
        evidence.as_ref(),
        judge,
        state,
        deadline,
        &mut settled,
    );
    if edit(state, |brief| brief.silenced) {
        return;
    }
    if let Some(admission) = refusal(signal, &got, true, ENFORCE_GATE_ON_WEAK) {
        close(state, &got, admission);
        return;
    }
    let (change_intent, label) = match got.judged.as_ref().and_then(Option::as_ref) {
        Some(verdict) => (verdict.change_intent(), Some(verdict.label.clone())),
        None => (has_change_intent(typed), None),
    };
    let plan = Plan::from_typed(typed, change_intent, label.as_deref());
    fold(state, &plan, &got);
    run(&plan, evidence.as_ref(), state, deadline);
}

/// What the probes of a weak or plain prompt have answered so far.
#[derive(Default)]
struct Gathered {
    relevance: Option<Result<RelevanceAnswer, String>>,
    /// The decision scored from an answered relevance probe.
    scored: Option<relevance::Verdict>,
    meaning: Option<Result<Vec<MeaningHit>, String>>,
    /// `Some(None)`: the judge ran and had no verdict.
    judged: Option<Option<Verdict>>,
}

/// One probe's answer on its way back to the worker.
enum Reply {
    Relevance(Result<RelevanceAnswer, String>),
    Meaning(Result<Vec<MeaningHit>, String>),
    Judge(Option<Verdict>),
}

/// Ask the judge, the relevance probe and the meaning search at once, each
/// on the shared `deadline`; the probes count against the op budget. Stops
/// collecting as soon as `settled` says the brief is decided, so a refusal
/// does not wait for the slowest sibling.
fn gather<J>(
    typed: &str,
    evidence: &dyn Evidence,
    judge: &J,
    state: &Mutex<Brief>,
    deadline: Instant,
    settled: &mut dyn FnMut(&Gathered) -> bool,
) -> Gathered
where
    J: Fn(&str, Instant) -> Option<Verdict> + Sync,
{
    let ask_relevance = spend(state, deadline);
    let ask_meaning = spend(state, deadline);
    let mut got = Gathered::default();
    std::thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        if ask_relevance {
            let send = send.clone();
            scope.spawn(move || {
                let _ = send.send(Reply::Relevance(evidence.relevance(typed, deadline)));
            });
        }
        if ask_meaning {
            let send = send.clone();
            scope.spawn(move || {
                let _ = send.send(Reply::Meaning(evidence.meaning(
                    typed,
                    MEANING_LIMIT,
                    deadline,
                )));
            });
        }
        scope.spawn(move || {
            let _ = send.send(Reply::Judge(judge(typed, deadline)));
        });
        while let Ok(reply) =
            receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            match reply {
                Reply::Relevance(answer) => {
                    got.scored = answer
                        .as_ref()
                        .ok()
                        .map(|answer| relevance::judge(&answer.input));
                    got.relevance = Some(answer);
                }
                Reply::Meaning(leads) => got.meaning = Some(leads),
                Reply::Judge(verdict) => got.judged = Some(verdict),
            }
            if settled(&got) {
                break;
            }
        }
    });
    got
}

/// Why `signal`'s prompt gets no brief, if it does not. `complete`: no more
/// answers are coming, so a probe that never answered counts as "cannot tell".
/// Plain language must be shown on topic by the repository; a weak prompt is
/// refused by the judge, and by the relevance decision only when `enforce_weak`.
fn refusal(
    signal: Signal,
    got: &Gathered,
    complete: bool,
    enforce_weak: bool,
) -> Option<Admission> {
    let verdict = got.judged.as_ref().and_then(Option::as_ref);
    match signal {
        Signal::Strong => None,
        Signal::Weak => {
            if let Some(verdict) = verdict.filter(|verdict| verdict.denies_brief()) {
                return Some(Admission::Denied(judge_reason(verdict)));
            }
            got.scored
                .as_ref()
                .filter(|scored| enforce_weak && !scored.on_topic)
                .map(|scored| Admission::Closed(off_topic_reason(scored)))
        }
        Signal::Prose => {
            match (&got.relevance, &got.scored) {
                (Some(Err(reason)), _) => {
                    return Some(Admission::Closed(format!("relevance: {reason}")));
                }
                (_, Some(scored)) if !scored.on_topic => {
                    return Some(Admission::Closed(off_topic_reason(scored)));
                }
                (None, _) if complete => {
                    return Some(Admission::Closed(
                        "relevance: no answer before the deadline".to_string(),
                    ));
                }
                _ => {}
            }
            verdict
                .filter(|verdict| verdict.denies_prose())
                .map(|verdict| Admission::Denied(judge_reason(verdict)))
        }
    }
}

fn judge_reason(verdict: &Verdict) -> String {
    format!("judge: {} ({:.2})", verdict.label, verdict.confidence)
}

fn off_topic_reason(scored: &relevance::Verdict) -> String {
    format!(
        "off topic: the best file covers {}/{} key terms, {:.2} of the weight, {}",
        scored.features.shared,
        scored.features.informative,
        scored.score,
        if scored.features.structural {
            "with a symbol or path match"
        } else {
            "in text only"
        }
    )
}

/// Keep the decisions the probes reached: the relevance verdict and the judge's.
fn note(brief: &mut Brief, got: &Gathered) {
    brief.relevance.clone_from(&got.scored);
    if let Some(Some(verdict)) = &got.judged {
        brief.intent = Some(format!("{} ({:.2})", verdict.label, verdict.confidence));
        brief.judged = Some(verdict.clone());
    }
}

/// End the brief on a refusal that binds: nothing renders.
fn close(state: &Mutex<Brief>, got: &Gathered, admission: Admission) {
    edit(state, |brief| {
        note(brief, got);
        brief.admission = admission;
        brief.silenced = true;
    });
}

/// Fold what the probes found into the brief: the decision, and the fused
/// leads as its first files, so the literal and concept searches only run
/// when they found nothing.
fn fold(state: &Mutex<Brief>, plan: &Plan, got: &Gathered) {
    let leads: &[MeaningHit] = match &got.meaning {
        Some(Ok(leads)) => leads,
        _ => &[],
    };
    let lines: &[RichHit] = match &got.relevance {
        Some(Ok(answer)) => &answer.lines,
        _ => &[],
    };
    let fused = RichFound {
        hits: fuse(leads, lines),
        capped: false,
    };
    let admit_json = plan.kind == QuestionKind::Config;
    edit(state, |brief| {
        note(brief, got);
        brief.admission = match &got.scored {
            Some(scored) if scored.on_topic => Admission::Open,
            Some(scored) => Admission::Closed(off_topic_reason(scored)),
            None => Admission::Unjudged,
        };
        let answers = usize::from(matches!(got.relevance, Some(Ok(_))))
            + usize::from(matches!(got.meaning, Some(Ok(_))));
        if answers > 0 {
            brief.answered += answers;
            brief.searched = true;
        }
        brief.confidence = got.scored.as_ref().map(relevance::Verdict::confidence_line);
        brief.absorb(fused, admit_json);
    });
}

/// What the entry at zero-based `rank` of one list adds to a file's fused
/// score: ranks count from one, so the first entry adds `1 / (FUSE_K + 1)`.
fn reciprocal_rank(rank: usize) -> f64 {
    1.0 / (FUSE_K + rank as f64 + 1.0)
}

/// Reciprocal-rank fusion of the meaning leads and the relevance co-files,
/// best first and at most [`MAX_FILES`], one row per file. A file both lists
/// name rises above one only a list names; its row is the first list's, the
/// meaning chunk, which is the more precise place to start. A credential-shaped
/// path never enters a brief.
fn fuse(leads: &[MeaningHit], cofiles: &[RichHit]) -> Vec<RichHit> {
    fn add(ranked: &mut Vec<(RichHit, f64)>, hit: RichHit, rank: usize) {
        let score = reciprocal_rank(rank);
        match ranked.iter_mut().find(|(held, _)| held.path == hit.path) {
            Some((_, total)) => *total += score,
            None => ranked.push((hit, score)),
        }
    }
    let mut ranked: Vec<(RichHit, f64)> = Vec::new();
    for (rank, lead) in leads.iter().enumerate() {
        let hit = RichHit {
            path: lead.path.clone(),
            line: u64::from(lead.start_line),
            text: (!lead.snippet.is_empty()).then(|| lead.snippet.clone()),
        };
        add(&mut ranked, hit, rank);
    }
    for (rank, hit) in cofiles.iter().enumerate() {
        add(&mut ranked, hit.clone(), rank);
    }
    ranked.retain(|(hit, _)| !pixel_index::index::credential_path(Path::new(&hit.path)));
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    ranked
        .into_iter()
        .map(|(hit, _)| hit)
        .take(MAX_FILES)
        .collect()
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
        }
    }

    /// Drop one entry from the longest list (the earlier of equals in the
    /// order excluded, files, tests, targets, skeleton, history, callers,
    /// defined, caps, unresolved); `false` when every list is already empty.
    fn shrink(&mut self) -> bool {
        let widest = [
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

/// The block, at most [`byte_cap`] bytes, or `None` when nothing answered or
/// the gate refused the prompt. A list that does not fit loses entries and
/// says how many.
pub(crate) fn render(brief: &Brief) -> Option<String> {
    if brief.silenced || brief.answered == 0 {
        return None;
    }
    let cap = byte_cap(brief.signal);
    let mut shown = Shown::of(brief);
    let mut text = render_with(brief, shown);
    while text.len() > cap && shown.shrink() {
        text = render_with(brief, shown);
    }
    Some(text)
}

/// The size cap of a brief started by `signal`.
fn byte_cap(signal: Option<Signal>) -> usize {
    if signal == Some(Signal::Prose) {
        PROSE_BRIEF_BYTES
    } else {
        BRIEF_BYTES
    }
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
    if let Some(confidence) = &brief.confidence {
        lines.push(confidence.clone());
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
        relevance: Result<RelevanceAnswer, String>,
        meaning: Result<Vec<MeaningHit>, String>,
        /// How long the meaning search takes, on top of `pause`.
        meaning_pause: Duration,
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
                relevance: Err("unavailable".into()),
                meaning: Err("unavailable".into()),
                meaning_pause: Duration::ZERO,
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
        fn relevance(&self, typed: &str, _: Instant) -> Result<RelevanceAnswer, String> {
            self.note(format!("relevance {typed}"));
            self.relevance.clone()
        }
        fn meaning(
            &self,
            query: &str,
            limit: usize,
            _: Instant,
        ) -> Result<Vec<MeaningHit>, String> {
            self.note(format!("meaning {query} limit {limit}"));
            std::thread::sleep(self.meaning_pause);
            self.meaning.clone()
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

    /// A `Fake` the test keeps a handle to after the brief took the source.
    struct Shared(Arc<Fake>);

    impl Evidence for Shared {
        fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
            self.0.files_with(anchor, deadline)
        }
        fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
            self.0.concept(phrase, deadline)
        }
        fn symbols(&self, name: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String> {
            self.0.symbols(name, deadline)
        }
        fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String> {
            self.0.callers(target, deadline)
        }
        fn relevance(&self, typed: &str, deadline: Instant) -> Result<RelevanceAnswer, String> {
            self.0.relevance(typed, deadline)
        }
        fn meaning(
            &self,
            query: &str,
            limit: usize,
            deadline: Instant,
        ) -> Result<Vec<MeaningHit>, String> {
            self.0.meaning(query, limit, deadline)
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
            ..Brief::default()
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
        brief.files = (0..8)
            .map(|n| rhit_no_text(&format!("{}{n}", "d".repeat(MAX_ITEM_CHARS - 1)), 1))
            .collect();
        brief.callers = (0..10)
            .map(|n| {
                caller(
                    &format!("{}{n}", "c".repeat(MAX_ITEM_CHARS - 1)),
                    &"v".repeat(60),
                    1,
                )
            })
            .collect();
        brief.unresolved = (0..10)
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

    // ---- plain-language and weak prompts: the relevance gate ----

    /// A plain-language prompt: no identifier, no code word, no operation.
    const PROSE: &str = "the daemon misses changes made during startup";
    const DAEMON: &str = "crates/pixel-daemon/src/daemon.rs";

    fn keyword(word: &str, weight: f64) -> relevance::KeywordStat {
        relevance::KeywordStat {
            keyword: word.into(),
            weight,
            french_only: false,
        }
    }

    fn cofile(path: &str, words: &[&str], weight: f64, structural: bool) -> relevance::CoFileStat {
        relevance::CoFileStat {
            path: path.into(),
            keywords: words.iter().map(ToString::to_string).collect(),
            weight,
            structural,
        }
    }

    /// The words of a prompt the repository talks about, as many as one file
    /// must share for the gate to open.
    const TOPIC_WORDS: [&str; 6] = ["daemon", "changes", "startup", "restart", "files", "watch"];

    /// The repository talks about the prompt: one structural file holds all
    /// its words (2 each), a note holds all but one as text.
    fn on_topic_answer() -> RelevanceAnswer {
        let count = relevance::MIN_SHARED_KEYWORDS;
        let topic = &TOPIC_WORDS[..count];
        RelevanceAnswer {
            input: RelevanceInput {
                graph: true,
                keywords: topic.iter().map(|word| keyword(word, 2.0)).collect(),
                cofiles: vec![
                    cofile(DAEMON, topic, 2.0 * count as f64, true),
                    cofile(
                        "docs/notes.md",
                        &topic[..count - 1],
                        2.0 * (count - 1) as f64,
                        false,
                    ),
                ],
            },
            lines: vec![
                rhit(DAEMON, 280, "fn watch_ready"),
                rhit("docs/notes.md", 12, "daemon startup notes"),
            ],
        }
    }

    /// The `confidence:` line of a brief the whole of `on_topic_answer` covers.
    fn full_confidence() -> String {
        let count = relevance::MIN_SHARED_KEYWORDS;
        format!("confidence: high — {count}/{count} key terms covered; start with the first file")
    }

    /// The repository does not talk about the prompt: two of its words are
    /// nowhere (the cap), the third is everywhere (0), and no file holds any.
    fn off_topic_answer() -> RelevanceAnswer {
        RelevanceAnswer {
            input: RelevanceInput {
                graph: true,
                keywords: vec![
                    keyword("weather", 3.0),
                    keyword("tomorrow", 3.0),
                    keyword("like", 0.0),
                ],
                cofiles: Vec::new(),
            },
            lines: Vec::new(),
        }
    }

    fn lead(path: &str, line: u32, snippet: &str) -> MeaningHit {
        MeaningHit {
            path: path.into(),
            start_line: line,
            end_line: line + 9,
            symbol: None,
            score: 0.5,
            snippet: snippet.into(),
        }
    }

    fn sorted(mut calls: Vec<String>) -> Vec<String> {
        calls.sort();
        calls
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-brief-chain-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Start `prompt` on `fake` with `judge` and wait for the brief.
    fn briefed(
        prompt: &str,
        fake: Fake,
        judge: impl Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
    ) -> Finished {
        start_with(prompt, OPEN, SECOND, move |_| Box::new(fake), judge)
            .expect("the prompt starts a brief")
            .finish_with_record()
    }

    #[test]
    fn a_prose_prompt_on_topic_should_render_the_fused_files_and_a_confidence_line() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![
            lead(DAEMON, 280, "fn watch_ready() {"),
            lead("crates/pixel-daemon/src/api.rs", 40, "fn op_status"),
        ]);
        let fake = Arc::new(fake);
        let seen = Arc::clone(&fake);
        let finished = start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(Shared(fake)),
            no_verdict,
        )
        .unwrap()
        .finish_with_record();
        // The file both lists name leads, on the meaning chunk's line; then
        // the two files one list names, the meaning lead first.
        assert_eq!(
            finished.text.as_deref(),
            Some(
                [
                    "[PIXEL:BRIEF]",
                    "kind: lookup",
                    "files: crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready() {; crates/pixel-daemon/src/api.rs:40 — fn op_status; docs/notes.md:12 — daemon startup notes",
                    full_confidence().as_str(),
                    "coverage: 2/2 ops answered",
                    FOOTER,
                ]
                .join("\n")
                .as_str()
            )
        );
        assert_eq!(
            sorted(seen.calls()),
            [
                format!("meaning {PROSE} limit 8"),
                format!("relevance {PROSE}")
            ]
        );
        let record = finished.record;
        assert_eq!(
            (record.signal, record.gate, record.enforced),
            (Some("prose"), "open", true)
        );
        assert_eq!(record.best_file.as_deref(), Some(DAEMON));
        assert!((record.score.unwrap() - 1.0).abs() < f64::EPSILON);
        assert_eq!((record.ops, record.answered), (2, 2));
        let features = record.features.as_ref().unwrap();
        assert_eq!(
            (features.shared, features.informative),
            (
                relevance::MIN_SHARED_KEYWORDS,
                relevance::MIN_SHARED_KEYWORDS
            )
        );
        assert!(features.structural);
        assert_eq!(record.bytes, finished.text.unwrap().len());
        assert_eq!(record.typed, PROSE);
        assert_eq!(record.sha256, decision_log::sha256_hex(PROSE));
    }

    #[test]
    fn a_prose_prompt_off_topic_should_render_nothing_without_waiting_for_the_slow_meaning_search()
    {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        fake.meaning_pause = Duration::from_millis(1200);
        let started = Instant::now();
        let finished = start_with(
            "what's the weather going to be like tomorrow",
            OPEN,
            Duration::from_secs(5),
            move |_| Box::new(fake),
            no_verdict,
        )
        .unwrap()
        .finish_with_record();
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(finished.text, None);
        let record = finished.record;
        assert_eq!((record.signal, record.gate), (Some("prose"), "closed"));
        assert_eq!(
            record.reason.as_deref(),
            Some("off topic: the best file covers 0/2 key terms, 0.00 of the weight, in text only")
        );
        assert_eq!((record.answered, record.bytes), (0, 0));
    }

    #[test]
    fn a_prose_prompt_a_confident_none_or_ops_verdict_refuses_should_render_nothing() {
        for label in ["none", "ops"] {
            let mut fake = Fake::new();
            fake.relevance = Ok(on_topic_answer());
            fake.meaning = Ok(vec![lead(DAEMON, 280, "fn watch_ready")]);
            let finished = briefed(PROSE, fake, verdict(label));
            assert_eq!(finished.text, None, "{label}");
            assert_eq!(finished.record.gate, "denied", "{label}");
            assert_eq!(
                finished.record.reason,
                Some(format!("judge: {label} (0.90)")),
                "{label}"
            );
            assert_eq!(
                finished.record.judge,
                Some((label.to_string(), 0.9)),
                "{label}"
            );
        }
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let finished = briefed(PROSE, fake, verdict("question"));
        let text = finished
            .text
            .expect("a question verdict lets the brief through");
        assert!(text.contains("\nintent: question (0.90)\n"), "{text}");
        assert_eq!(finished.record.gate, "open");
    }

    #[test]
    fn a_prose_prompt_whose_probes_all_fail_should_render_nothing() {
        let finished = briefed(PROSE, Fake::new(), no_verdict);
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.gate, "closed");
        assert_eq!(
            finished.record.reason.as_deref(),
            Some("relevance: unavailable")
        );
        assert_eq!((finished.record.ops, finished.record.answered), (2, 0));
    }

    #[test]
    fn a_prose_prompt_should_still_render_the_cofiles_when_the_meaning_search_fails() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Err("cold".into());
        let finished = briefed(PROSE, fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(
            text.contains(
                "\nfiles: crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready; docs/notes.md:12 — daemon startup notes\n"
            ),
            "{text}"
        );
        assert!(text.contains("\ncoverage: 1/2 ops answered\n"), "{text}");
        assert!(!text.contains("unresolved"), "{text}");
    }

    #[test]
    fn a_prose_prompt_whose_relevance_probe_outlives_the_window_should_render_nothing_on_time() {
        struct Hang;
        impl Evidence for Hang {
            fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
                Err("unused".into())
            }
            fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
                Err("unused".into())
            }
            fn symbols(&self, _: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
                Err("unused".into())
            }
            fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
                Err("unused".into())
            }
            fn relevance(&self, _: &str, _: Instant) -> Result<RelevanceAnswer, String> {
                std::thread::sleep(Duration::from_millis(500));
                Ok(on_topic_answer())
            }
        }
        let started = Instant::now();
        let pending = start_with(
            PROSE,
            OPEN,
            Duration::from_millis(150),
            |_| Box::new(Hang),
            no_verdict,
        )
        .unwrap();
        assert_eq!(pending.finish(), None);
        assert!(
            started.elapsed() < Duration::from_millis(450),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_weak_prompt_should_take_the_probe_leads_and_skip_concept_matching() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(
            "crates/pixel-daemon/src/api.rs",
            40,
            "fn op_status",
        )]);
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let finished = briefed("how does the login flow work", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(
            text.contains(
                "\nfiles: crates/pixel-daemon/src/api.rs:40 — fn op_status; crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready; docs/notes.md:12 — daemon startup notes\n"
            ),
            "{text}"
        );
        assert!(!text.contains("src/login.ts"), "{text}");
        assert_eq!(finished.record.signal, Some("weak"));
        assert_eq!(finished.record.gate, "open");
    }

    #[test]
    fn a_weak_prompt_whose_probes_fail_should_fall_back_to_concept_matching_quietly() {
        let mut fake = Fake::new();
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let finished = briefed("how does the login flow work", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(text.contains("\nfiles: src/login.ts:3\n"), "{text}");
        assert!(!text.contains("unresolved"), "{text}");
        assert!(!text.contains("confidence"), "{text}");
        assert!(text.ends_with(FOOTER), "{text}");
        assert_eq!(finished.record.gate, "unjudged");
    }

    #[test]
    fn a_weak_prompt_an_off_topic_decision_flags_should_still_get_its_brief_while_unenforced() {
        // Flipping `ENFORCE_GATE_ON_WEAK` flips this test: the decision is
        // logged either way, and the const says whether it binds.
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let finished = briefed("how does the login flow work", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(text.contains("\nfiles: src/login.ts:3\n"), "{text}");
        assert!(
            text.contains(
                "\nconfidence: low — 0/2 key terms covered; verify with rg before relying on these files\n"
            ),
            "{text}"
        );
        assert_eq!(finished.record.gate, "closed");
        assert!(!finished.record.enforced);
    }

    #[test]
    fn a_strong_prompt_should_never_ask_the_relevance_probe_or_the_meaning_search() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        let fake = Arc::new(fake);
        let seen = Arc::clone(&fake);
        let finished = start_with(
            "where is `fetchUser` used",
            OPEN,
            SECOND,
            move |_| Box::new(Shared(fake)),
            no_verdict,
        )
        .unwrap()
        .finish_with_record();
        assert!(
            seen.calls()
                .iter()
                .all(|call| !call.starts_with("relevance") && !call.starts_with("meaning")),
            "{:?}",
            seen.calls()
        );
        assert_eq!(finished.record.signal, Some("strong"));
        assert_eq!(finished.record.gate, "unjudged");
        assert!(!finished.record.enforced);
        assert_eq!(finished.record.score, None);
    }

    #[test]
    fn gather_should_spend_one_op_per_probe_and_skip_a_probe_the_budget_refuses() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        let state = Mutex::new(Brief {
            ops: MAX_OPS - 1,
            ..Brief::default()
        });
        let got = gather(
            PROSE,
            &fake,
            &no_verdict,
            &state,
            Instant::now() + SECOND,
            &mut |_| false,
        );
        assert_eq!(fake.calls(), [format!("relevance {PROSE}")]);
        assert!(matches!(got.relevance, Some(Ok(_))));
        assert!(got.meaning.is_none());
        assert_eq!(got.judged, Some(None));
        let brief = edit(&state, |brief| brief.clone());
        assert_eq!((brief.ops, brief.cut), (MAX_OPS, true));

        let fresh = Mutex::new(Brief::default());
        let both = Fake::new();
        gather(
            PROSE,
            &both,
            &no_verdict,
            &fresh,
            Instant::now() + SECOND,
            &mut |_| false,
        );
        assert_eq!(edit(&fresh, |brief| brief.ops), 2);
        assert_eq!(
            sorted(both.calls()),
            [
                format!("meaning {PROSE} limit 8"),
                format!("relevance {PROSE}")
            ]
        );
    }

    #[test]
    fn gather_should_stop_collecting_when_the_decision_is_settled() {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        let state = Mutex::new(Brief::default());
        let mut seen = 0;
        let got = gather(
            PROSE,
            &fake,
            &no_verdict,
            &state,
            Instant::now() + SECOND,
            &mut |_| {
                seen += 1;
                true
            },
        );
        assert_eq!(seen, 1, "the first reply settled it");
        let replies = usize::from(got.relevance.is_some())
            + usize::from(got.meaning.is_some())
            + usize::from(got.judged.is_some());
        assert_eq!(replies, 1);
    }

    fn gathered(
        relevance: Option<Result<RelevanceAnswer, String>>,
        judged: Option<Option<Verdict>>,
    ) -> Gathered {
        let scored = relevance
            .as_ref()
            .and_then(|answer| answer.as_ref().ok())
            .map(|answer| relevance::judge(&answer.input));
        Gathered {
            relevance,
            scored,
            meaning: None,
            judged,
        }
    }

    fn said(label: &str, confidence: f64) -> Option<Option<Verdict>> {
        Some(Some(Verdict {
            label: label.into(),
            confidence,
        }))
    }

    #[test]
    fn refusal_should_let_a_strong_prompt_through_whatever_the_probes_say() {
        let got = gathered(Some(Ok(off_topic_answer())), said("none", 0.99));
        assert_eq!(refusal(Signal::Strong, &got, true, true), None);
    }

    #[test]
    fn refusal_should_close_a_prose_prompt_the_repository_does_not_cover() {
        let off = gathered(Some(Ok(off_topic_answer())), None);
        assert_eq!(
            refusal(Signal::Prose, &off, false, false),
            Some(Admission::Closed(
                "off topic: the best file covers 0/2 key terms, 0.00 of the weight, in text only"
                    .into()
            ))
        );
        let failed = gathered(Some(Err("cold".into())), None);
        assert_eq!(
            refusal(Signal::Prose, &failed, false, false),
            Some(Admission::Closed("relevance: cold".into()))
        );
    }

    #[test]
    fn refusal_should_wait_for_a_missing_relevance_answer_until_nothing_more_is_coming() {
        let silent = gathered(None, Some(None));
        assert_eq!(refusal(Signal::Prose, &silent, false, false), None);
        assert_eq!(
            refusal(Signal::Prose, &silent, true, false),
            Some(Admission::Closed(
                "relevance: no answer before the deadline".into()
            ))
        );
    }

    #[test]
    fn refusal_should_deny_prose_only_on_a_confident_none_or_ops() {
        let on = || Some(Ok(on_topic_answer()));
        assert_eq!(
            refusal(
                Signal::Prose,
                &gathered(on(), said("none", 0.5)),
                true,
                false
            ),
            Some(Admission::Denied("judge: none (0.50)".into()))
        );
        assert_eq!(
            refusal(
                Signal::Prose,
                &gathered(on(), said("ops", 0.9)),
                true,
                false
            ),
            Some(Admission::Denied("judge: ops (0.90)".into()))
        );
        assert_eq!(
            refusal(
                Signal::Prose,
                &gathered(on(), said("none", 0.49)),
                true,
                false
            ),
            None
        );
        assert_eq!(
            refusal(
                Signal::Prose,
                &gathered(on(), said("bugfix", 0.99)),
                true,
                false
            ),
            None
        );
        // The judge may refuse before the relevance answer has arrived.
        assert_eq!(
            refusal(
                Signal::Prose,
                &gathered(None, said("none", 0.9)),
                false,
                false
            ),
            Some(Admission::Denied("judge: none (0.90)".into()))
        );
    }

    #[test]
    fn refusal_should_deny_a_weak_prompt_only_on_none_and_close_it_only_when_enforced() {
        let on = || Some(Ok(on_topic_answer()));
        let off = || Some(Ok(off_topic_answer()));
        assert_eq!(
            refusal(
                Signal::Weak,
                &gathered(on(), said("none", 0.9)),
                true,
                false
            ),
            Some(Admission::Denied("judge: none (0.90)".into()))
        );
        assert_eq!(
            refusal(
                Signal::Weak,
                &gathered(on(), said("ops", 0.99)),
                true,
                false
            ),
            None,
            "ops steers a weak prompt, it never silences it"
        );
        assert_eq!(
            refusal(Signal::Weak, &gathered(off(), None), true, false),
            None
        );
        assert_eq!(
            refusal(Signal::Weak, &gathered(off(), None), true, true),
            Some(Admission::Closed(
                "off topic: the best file covers 0/2 key terms, 0.00 of the weight, in text only"
                    .into()
            ))
        );
        assert_eq!(
            refusal(Signal::Weak, &gathered(on(), None), true, true),
            None
        );
        let failed = gathered(Some(Err("cold".into())), None);
        assert_eq!(
            refusal(Signal::Weak, &failed, true, true),
            None,
            "a probe that cannot tell never silences a weak prompt"
        );
        assert_eq!(
            refusal(Signal::Weak, &gathered(None, None), true, true),
            None
        );
    }

    #[test]
    fn fuse_should_rank_a_file_both_lists_name_above_one_list_and_keep_the_meaning_row() {
        let fused = fuse(
            &[
                lead("a.rs", 10, "from meaning"),
                lead("b.rs", 20, "b meaning"),
            ],
            &[rhit("b.rs", 99, "from cofile"), rhit("c.rs", 5, "c cofile")],
        );
        assert_eq!(
            fused,
            [
                rhit("b.rs", 20, "b meaning"),
                rhit("a.rs", 10, "from meaning"),
                rhit("c.rs", 5, "c cofile"),
            ]
        );
    }

    #[test]
    fn fuse_should_keep_ties_in_list_order_meaning_first() {
        let fused = fuse(
            &[lead("m1.rs", 1, "m1"), lead("m2.rs", 2, "m2")],
            &[rhit("c1.rs", 1, "c1"), rhit("c2.rs", 2, "c2")],
        );
        let paths: Vec<&str> = fused.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(paths, ["m1.rs", "c1.rs", "m2.rs", "c2.rs"]);
    }

    #[test]
    fn fuse_should_give_a_leadless_row_no_text_and_stop_at_the_file_cap() {
        let leads: Vec<MeaningHit> = (0..MAX_FILES + 3)
            .map(|n| lead(&format!("f{n}.rs"), 1, if n == 0 { "" } else { "x" }))
            .collect();
        let fused = fuse(&leads, &[]);
        assert_eq!(fused.len(), MAX_FILES);
        assert_eq!(fused[0], rhit_no_text("f0.rs", 1));
        assert_eq!(fused[MAX_FILES - 1].path, format!("f{}.rs", MAX_FILES - 1));
        assert!(fuse(&[], &[]).is_empty());
    }

    #[test]
    fn fuse_should_never_name_a_credential_shaped_path() {
        let fused = fuse(
            &[lead(".aws/credentials", 1, "key"), lead("src/a.rs", 2, "a")],
            &[rhit(".env", 3, "TOKEN=1")],
        );
        assert_eq!(fused, [rhit("src/a.rs", 2, "a")]);
    }

    #[test]
    fn a_brief_the_gate_silenced_should_render_nothing_even_when_it_answered() {
        let mut brief = full_brief();
        assert!(render(&brief).is_some());
        brief.silenced = true;
        assert_eq!(render(&brief), None);
    }

    #[test]
    fn the_confidence_line_should_follow_the_files_line() {
        let mut brief = full_brief();
        brief.confidence =
            Some("confidence: high — 3/3 key terms covered; start with the first file".into());
        let text = render(&brief).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let files = lines
            .iter()
            .position(|line| line.starts_with("files: "))
            .unwrap();
        assert_eq!(
            lines[files + 1],
            "confidence: high — 3/3 key terms covered; start with the first file"
        );
    }

    #[test]
    fn byte_cap_should_give_a_prose_brief_its_own_bound() {
        assert_eq!(byte_cap(Some(Signal::Prose)), PROSE_BRIEF_BYTES);
        assert_eq!(byte_cap(Some(Signal::Strong)), BRIEF_BYTES);
        assert_eq!(byte_cap(Some(Signal::Weak)), BRIEF_BYTES);
        assert_eq!(byte_cap(None), BRIEF_BYTES);
        assert_eq!(PROSE_BRIEF_BYTES, 2048);
    }

    #[test]
    fn a_prose_brief_should_fit_its_cap() {
        let mut brief = full_brief();
        brief.signal = Some(Signal::Prose);
        brief.files = (0..30)
            .map(|n| {
                rhit(
                    &format!("crates/long/path/number/{n}/file.rs"),
                    n + 1,
                    "some matched text on the line",
                )
            })
            .collect();
        let text = render(&brief).unwrap();
        assert!(text.len() <= PROSE_BRIEF_BYTES, "{}", text.len());
        assert!(text.contains("(+"), "{text}");
    }

    #[test]
    fn admission_should_name_itself_and_its_reason_for_the_log() {
        assert_eq!(Admission::default(), Admission::Unjudged);
        for (admission, label, reason) in [
            (Admission::Unjudged, "unjudged", None),
            (Admission::Open, "open", None),
            (Admission::Closed("why".into()), "closed", Some("why")),
            (Admission::Denied("how".into()), "denied", Some("how")),
        ] {
            assert_eq!(admission.label(), label);
            assert_eq!(admission.reason(), reason);
        }
    }

    #[test]
    fn gate_enforced_should_bind_prose_always_and_a_weak_prompt_when_asked_or_refused() {
        assert!(gate_enforced(Signal::Prose, false, false));
        assert!(gate_enforced(Signal::Weak, true, false));
        assert!(gate_enforced(Signal::Weak, false, true));
        assert!(!gate_enforced(Signal::Weak, false, false));
        assert!(!gate_enforced(Signal::Strong, true, true));
    }

    #[test]
    fn declined_should_name_why_no_brief_started() {
        let closed = Gate {
            enabled: false,
            indexed: true,
        };
        let unindexed = Gate {
            enabled: true,
            indexed: false,
        };
        let open = |_: Instant| -> Box<dyn Evidence> { Box::new(Fake::new()) };
        let why = |prompt: &str, gate: Gate| {
            try_start_with(prompt, gate, SECOND, open, no_verdict, None)
                .err()
                .map(Declined::as_str)
        };
        assert_eq!(why("callers of `fetchUser`", closed), Some("disabled"));
        assert_eq!(why("callers of `fetchUser`", unindexed), Some("unindexed"));
        assert_eq!(why("ok", OPEN), Some("continuation"));
        assert_eq!(why("thanks, that works", OPEN), Some("not_about_code"));
        assert_eq!(why("commit and push", OPEN), Some("not_about_code"));
        assert_eq!(why("callers of `fetchUser`", OPEN), None);
        assert_eq!(why(PROSE, OPEN), None);
    }

    #[test]
    fn declined_should_spell_every_reason_for_the_log() {
        let spelled: Vec<&str> = [
            Declined::Disabled,
            Declined::Unindexed,
            Declined::Continuation,
            Declined::NotAboutCode,
            Declined::NoWorker,
        ]
        .into_iter()
        .map(Declined::as_str)
        .collect();
        assert_eq!(
            spelled,
            [
                "disabled",
                "unindexed",
                "continuation",
                "not_about_code",
                "no_worker"
            ]
        );
    }

    #[test]
    fn a_decision_should_be_logged_only_when_a_log_is_given() {
        let dir = scratch_dir("given");
        let path = dir.join(decision_log::LOG_FILE);
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let finished = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
            Some(path.clone()),
        )
        .unwrap()
        .finish_with_record();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap(),
            finished.record.to_json()
        );
        // The same decision with no log leaves nothing behind.
        let quiet = scratch_dir("absent");
        let mut other = Fake::new();
        other.relevance = Ok(on_topic_answer());
        try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(other),
            no_verdict,
            None,
        )
        .unwrap()
        .finish_with_record();
        assert!(std::fs::read_dir(&quiet).unwrap().next().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&quiet).unwrap();
    }

    #[test]
    fn a_prompt_about_nothing_should_be_logged_as_declined_and_a_continuation_should_not() {
        let dir = scratch_dir("declined");
        let path = dir.join(decision_log::LOG_FILE);
        let open = |_: Instant| -> Box<dyn Evidence> { Box::new(Fake::new()) };
        for prompt in ["ok", "thanks, that works"] {
            assert!(
                try_start_with(prompt, OPEN, SECOND, open, no_verdict, Some(path.clone())).is_err()
            );
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert_eq!(lines[0]["gate"], "declined");
        assert_eq!(lines[0]["reason"], "not_about_code");
        assert_eq!(lines[0]["typed"], "thanks, that works");
        assert_eq!(lines[0]["signal"], serde_json::Value::Null);
        // A gate that never opened writes nothing, whatever the prompt.
        let closed = Gate {
            enabled: false,
            indexed: true,
        };
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(
            try_start_with(
                "thanks, that works",
                closed,
                SECOND,
                open,
                no_verdict,
                Some(path.clone())
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_log_should_keep_the_last_five_hundred_decisions() {
        let dir = scratch_dir("capped");
        let path = dir.join(decision_log::LOG_FILE);
        let old: String = (0..decision_log::MAX_LINES)
            .map(|n| format!("{{\"old\":{n}}}\n"))
            .collect();
        std::fs::write(&path, old).unwrap();
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
            Some(path.clone()),
        )
        .unwrap()
        .finish_with_record();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 500);
        assert_eq!(lines[0], "{\"old\":1}");
        assert!(
            lines[499].contains("\"signal\":\"prose\""),
            "{}",
            lines[499]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn declined_record_should_carry_the_typed_text_only() {
        let record = declined_record(
            Declined::NotAboutCode,
            "<pasted_content>secret thread</pasted_content> thanks, that works",
        );
        assert_eq!(record.gate, "declined");
        assert_eq!(record.reason.as_deref(), Some("not_about_code"));
        assert_eq!(record.typed, "thanks, that works");
        assert_eq!(
            record.sha256,
            decision_log::sha256_hex("thanks, that works")
        );
        assert_eq!(record.signal, None);
        assert_eq!((record.ops, record.answered, record.bytes), (0, 0, 0));
        assert!(record.ts_ms > 1_577_836_800_000, "{}", record.ts_ms);
    }

    #[test]
    fn a_decision_should_be_stamped_with_the_clock_and_the_time_it_took() {
        let before = pixel_task::now_ms();
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.pause = Duration::from_millis(30);
        let finished = briefed(PROSE, fake, no_verdict);
        let after = pixel_task::now_ms();
        let record = finished.record;
        assert!(
            (before..=after).contains(&record.ts_ms),
            "{before} <= {} <= {after}",
            record.ts_ms
        );
        assert!(record.elapsed_ms >= 30, "{}", record.elapsed_ms);
        assert!(record.elapsed_ms < 1000, "{}", record.elapsed_ms);
    }

    #[test]
    fn reciprocal_rank_should_count_ranks_from_one() {
        assert!((reciprocal_rank(0) - 1.0 / 61.0).abs() < 1e-12);
        assert!((reciprocal_rank(1) - 1.0 / 62.0).abs() < 1e-12);
        assert!((reciprocal_rank(7) - 1.0 / 68.0).abs() < 1e-12);
    }

    #[test]
    fn a_weak_prompt_with_only_a_meaning_lead_should_show_it_without_a_confidence_line() {
        let mut fake = Fake::new();
        fake.meaning = Ok(vec![lead(
            "crates/pixel-daemon/src/api.rs",
            40,
            "fn op_status",
        )]);
        let finished = briefed("how does the login flow work", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(
            text.contains("\nfiles: crates/pixel-daemon/src/api.rs:40 — fn op_status\n"),
            "{text}"
        );
        assert!(!text.contains("confidence"), "{text}");
        assert!(text.contains("\ncoverage: 1/2 ops answered\n"), "{text}");
        assert_eq!(finished.record.gate, "unjudged");
        assert_eq!(finished.record.score, None);
    }

    #[test]
    fn a_weak_feature_prompt_with_every_search_failing_should_show_no_files_line() {
        let mut fake = Fake::new();
        fake.concept = Err("no index".into());
        fake.task_facts = Ok(vec!["src/new/route.ts".into()]);
        let finished = briefed(
            "add an export endpoint to the report page",
            fake,
            verdict("feature"),
        );
        let text = finished.text.unwrap();
        assert!(text.contains("\ntargets: src/new/route.ts\n"), "{text}");
        assert!(!text.contains("\nfiles:"), "{text}");
    }

    #[test]
    fn a_judge_that_refuses_before_the_relevance_probe_answers_should_be_the_reason() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.pause = Duration::from_millis(300);
        let started = Instant::now();
        let finished = briefed(PROSE, fake, verdict("none"));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.gate, "denied");
        assert_eq!(
            finished.record.reason.as_deref(),
            Some("judge: none (0.90)")
        );
    }
}
