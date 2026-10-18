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
//! there. A weakly code-shaped prompt gets the same probes as its retriever;
//! the same decision silences it when [`ENFORCE_GATE_ON_WEAK`] is set, and a
//! code graph that did not answer leaves its pre-gate brief in place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use super::answer::{self, Excerpt, Hop, Receipt};
use super::autostart;
use super::decision_log::{self, Record, Stages};
use super::feedback;
use super::intent::Verdict;
use super::kinds::{self, ChangeScope, Changes, FlowNode, KindInput, RepoMap};
use super::probes;
use super::region_pack;
use super::relevance::{self, Basis, GateInput, RelevanceInput, Tier};
use super::routes::QuestionKind;
use super::sent::{self, Seen};
use super::session_paths::{self, SessionPaths};
use super::{SOURCE_EXTENSIONS, Signal, brief_signal, names_code};

/// Opening line of the block; a host and a test find the brief by it.
pub(crate) const BRIEF_TAG: &str = "[PIXEL:BRIEF]";
/// Settings key of the opt-out (`brief: false` in `.pixel/config.yaml`).
pub(crate) const BRIEF_FEATURE: &str = "brief";
/// Environment opt-out: `0`, `false` or `off` silences the brief.
pub(crate) const BRIEF_ENV: &str = "PIXEL_BRIEF";
/// The one window shared by every operation of one brief, measured from the
/// moment the hook receives the prompt, for a brief started in `root`: three
/// seconds by default, because a machine running a dozen agent sessions slows
/// every lookup at once and a window that ends first leaves the agent no
/// brief at all. The environment (`PIXEL_BRIEF_WINDOW_MS`), then the
/// repository's and the global `brief_window_ms`, then the default of the
/// caller (the hook's, or the `brief` command's, which OpenCode cuts off sooner),
/// clamped to what the hook that runs the brief can wait for.
pub(crate) fn window_for(root: &Path, command: bool) -> Duration {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let default = if command {
        pixel_setup::config::BRIEF_WINDOW_COMMAND_DEFAULT_MS
    } else {
        pixel_setup::config::BRIEF_WINDOW_HOOK_DEFAULT_MS
    };
    window_of(
        pixel_setup::config::brief_window_ms(
            &home,
            Some(root),
            std::env::var(pixel_setup::config::BRIEF_WINDOW_ENV)
                .ok()
                .as_deref(),
            default,
        ),
        command,
    )
}

/// The window for a configured `ms`: the `brief` command is cut off by
/// OpenCode's extension after 1.2 s, so a setting tuned for the hook never lets it run past
/// [`pixel_setup::config::BRIEF_WINDOW_COMMAND_MAX_MS`].
fn window_of(ms: u64, command: bool) -> Duration {
    let cap = pixel_setup::config::BRIEF_WINDOW_COMMAND_MAX_MS;
    Duration::from_millis(if command { ms.min(cap) } else { ms })
}

/// Operations one brief may start, whatever they answer. A kind route may
/// use the headroom (search, concept, symbols, impact, context, one
/// kind op); the shared deadline, not this count, is the invariant.
pub(crate) const MAX_OPS: usize = 6;
/// Rendered size cap; lists give way before a line is cut.
pub(crate) const BRIEF_BYTES: usize = 2048;
/// Size cap of a confident brief that carries a search receipt or answer
/// excerpts (a plain-language prompt, or a weak one the gate put in the high
/// tier). OpenCode's extension caps injected context at 4000 bytes: stay under.
pub(crate) const PROSE_BRIEF_BYTES: usize = 3584;
const _: () = assert!(PROSE_BRIEF_BYTES < 4000);
/// Size cap of a code-shaped brief that names ranges to read.
pub(crate) const STRONG_READ_BRIEF_BYTES: usize = 2560;
/// Size cap of a plain-language brief without answer evidence.
pub(crate) const PLAIN_PROSE_BRIEF_BYTES: usize = 2048;
/// Files a brief that carries excerpts lists: the excerpts are the answer,
/// the list only says where else to look.
const ANSWER_FILES: usize = 4;
/// Files a generic excerpt covers.
const EXCERPT_FILES: usize = 3;
/// Whether an off-tier relevance decision silences a weakly code-shaped
/// prompt's brief, as it does a plain-language one. The decision is computed
/// and logged either way.
pub(crate) const ENFORCE_GATE_ON_WEAK: bool = true;
/// Files a low-tier brief names.
const LOW_FILES: usize = 3;
/// Rendered size cap of a low-tier brief: a maybe stays short.
pub(crate) const LOW_BRIEF_BYTES: usize = 700;
/// The same when the brief names ranges to read.
pub(crate) const LOW_READ_BRIEF_BYTES: usize = 1200;
/// The model that decides the tier of a prompt: [`relevance::judge`] in
/// production, a fixed answer in a test.
pub(crate) type Model = fn(&GateInput) -> relevance::Verdict;
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
/// The confidence line of a confident brief whose evidence did not earn the
/// ZERO step: the ranges below are the way to confirm it.
const MEDIUM_CONFIDENCE: &str = "confidence: medium — confirm in the ranges below";
/// The same for a brief with files and no range to read.
const MEDIUM_FILES_CONFIDENCE: &str = "confidence: medium — verify the files answer the question";
/// The footer of a brief with neither a ZERO step nor a range: the
/// verification rule without the directive.
const UNCORROBORATED_FOOTER: &str = "0 hits or 0 callers: verify with rg before concluding.";
/// The ZERO step: the excerpt shown is strong enough to answer from.
const ZERO_LINE: &str = "Answer from these lines; open a file only if they don't answer.";
/// The ONE-READ step: the ranges are the next, single action.
const READ_LINE: &str = "Read these ranges (in parallel, one turn) before any search. If they don't answer, search as usual.";
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryHit {
    pub(crate) sha: String,
    pub(crate) subject: String,
    /// When the commit was made (the index's timestamp), if it said.
    pub(crate) at: String,
    /// The path the hit is about (a path or diff hit), if any.
    pub(crate) path: Option<String>,
    /// The changed lines the hit matched (a diff hit), if any.
    pub(crate) snippet: Option<String>,
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
    /// Rows whose line matches the regular expression `pattern` (not
    /// escaped), at most `limit` of them in path order: what a locator asks
    /// for when it knows the shape of the line but not the exact text.
    fn grep(&self, pattern: &str, limit: usize, deadline: Instant) -> Result<RichFound, String> {
        let _ = (pattern, limit, deadline);
        Err("pattern search unsupported".to_string())
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
    /// The repository as the code graph has it: its indexed files, their
    /// top-level declarations and its `main` functions. Read-only, whole
    /// graph, no daemon op.
    fn repo_map(&self, _deadline: Instant) -> Result<RepoMap, String> {
        Err("repo map unsupported".to_string())
    }
    /// What changed in `scope`: the files of the diff, their line counts and
    /// the lines each changed. One bounded git call.
    fn changes(&self, scope: ChangeScope, _deadline: Instant) -> Result<Changes, String> {
        let _ = scope;
        Err("changes unsupported".to_string())
    }
    /// Functions and methods whose name contains one of `terms`, at most
    /// `limit`. Read-only, from the graph, no daemon op.
    fn find_symbols(
        &self,
        terms: &[String],
        limit: usize,
        _deadline: Instant,
    ) -> Result<Vec<SymbolHit>, String> {
        let _ = (terms, limit);
        Err("symbol search unsupported".to_string())
    }
    /// The calls out of the function `uid` (or a name that is only one), in
    /// the order the source makes them and depth first: at most `limit`
    /// nodes, `depth` levels deep.
    fn flow_tree(
        &self,
        uid: &str,
        depth: usize,
        limit: usize,
        _deadline: Instant,
    ) -> Result<Vec<FlowNode>, String> {
        let _ = (uid, depth, limit);
        Err("flow tree unsupported".to_string())
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
    /// The moved commands a plugin runs here. A `next:` line names a command
    /// the reader can run, so it names a plugin's only when the plugin runs;
    /// a source that does not know says none.
    fn plugins(&self) -> pixel_plugin::Available {
        pixel_plugin::Available::none()
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
    /// Where this source reads from, for the decision log: `daemon`, `local`,
    /// or `unknown` for a source that has no route.
    fn route(&self) -> &'static str {
        "unknown"
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
    /// At most `limit` leads for `query` ranked by its plain words (BM25 over
    /// the indexed files' names and paths, `words.rs`): what the brief fuses
    /// in place of the meaning leads when the meaning search did not answer.
    fn word_leads(
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
    /// Lines `start..=end` (1-based) of `path`: the bounded window an answer
    /// excerpt is cut from. Like `line_at`, a plain read outside the op
    /// budget; a credential-shaped path is refused.
    fn lines_at(
        &self,
        path: &str,
        start: u64,
        end: u64,
        _deadline: Instant,
    ) -> Result<Vec<String>, String> {
        let _ = (path, start, end);
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
        Self::read_with(root, BRIEF_ENV)
    }

    /// [`Gate::read`] with the name of the environment switch given.
    pub(crate) fn read_with(root: &Path, env: &str) -> Self {
        Self {
            enabled: crate::config_cmd::feature_enabled(Some(root), BRIEF_FEATURE, env),
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
            .or_else(|| self.sole_path_stem())
    }

    /// The stem of the one path a prompt names, as an identifier to search
    /// for. A prompt naming several files has no single name, and a stem such
    /// as `index` or `mod` is only a path segment, never a symbol.
    fn sole_path_stem(&self) -> Option<String> {
        match self.paths()[..] {
            [path] => Some(file_stem(path)).filter(|stem| !is_generic_stem(stem)),
            _ => None,
        }
    }

    /// The name `find-symbol` looks up: the last segment of a `a::b` word.
    fn symbol_name(&self) -> Option<String> {
        self.names()
            .next()
            .map(|name| name.rsplit("::").next().unwrap_or(name).to_string())
            .or_else(|| self.sole_path_stem())
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
        let heuristic = QuestionKind::heuristic(typed, &anchors);
        // A tour, a review and a walkthrough are read from the words; a
        // verdict that sees a task in them would send them to the lookup.
        let kind = if matches!(
            heuristic,
            QuestionKind::Overview | QuestionKind::Review | QuestionKind::Flow
        ) {
            heuristic
        } else {
            verdict_label
                .and_then(QuestionKind::of_verdict)
                .unwrap_or(heuristic)
        };
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

/// A file stem shared by countless files, so it names none of them.
fn is_generic_stem(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "index" | "mod" | "main" | "lib" | "init" | "__init__" | "__main__" | "types" | "utils"
    )
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
    /// The relevance gate would have closed the prompt, but it asks for a
    /// tour, a review or a walkthrough, which the repository's words cannot
    /// say: the kind admits it.
    Kind(QuestionKind),
    /// The relevance gate would have closed a short follow-up ("now fix the
    /// test"), but the session is already working on files: they stand for
    /// what the words cannot say.
    Context,
}

impl Admission {
    /// The decision as the log spells it.
    fn label(&self) -> &'static str {
        match self {
            Self::Unjudged => "unjudged",
            Self::Open => "open",
            Self::Closed(_) => "closed",
            Self::Denied(_) => "denied",
            Self::Kind(_) | Self::Context => "open",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Closed(reason) | Self::Denied(reason) => Some(reason),
            Self::Kind(kind) => Some(match kind {
                QuestionKind::Overview => "kind: overview",
                QuestionKind::Review => "kind: review",
                QuestionKind::Flow => "kind: flow",
                QuestionKind::History => "kind: history",
                _ => "kind",
            }),
            Self::Context => Some("context: a short follow-up on the session's files"),
            Self::Unjudged | Self::Open => None,
        }
    }
}

/// What the chain has learned, shared between the worker and the hook.
#[derive(Clone, Debug, Default)]
pub(crate) struct Brief {
    /// How the prompt came to be briefed.
    signal: Option<Signal>,
    /// Where the evidence came from: `daemon` or `local`.
    route: Option<&'static str>,
    admission: Admission,
    silenced: bool,
    /// The relevance decision, when the probe answered.
    relevance: Option<relevance::Verdict>,
    /// Ranges an earlier pack sent to this session.
    seen: Seen,
    /// A short follow-up the gate would have closed was let through on the
    /// session's files: the brief is a maybe, whatever the words scored.
    lifted: bool,
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
    /// The hop names of that flow, in order.
    flow_hops: Vec<String>,
    /// What the searches of this brief covered, when it is confident and the
    /// probe answered.
    receipt: Option<Receipt>,
    /// The typed question asks about tests, or about docs, changelogs and
    /// benchmarks: files of that kind are not demoted.
    wants_tests: bool,
    wants_docs: bool,
    /// The ZERO rule in force: a confident brief whose top chunk passes it
    /// tells the agent to answer from the excerpt. `None` when no rule is
    /// validated or `PIXEL_BRIEF_ZERO` is off.
    zero_rule: Option<answer::ZeroRule>,
    /// A code-shaped prompt resolved one short definition, shown as the answer.
    strong_zero: bool,
    /// The ranges to read, best first.
    reads: Vec<ReadRange>,
    /// The meaning search's leads, best first.
    leads: Vec<MeaningHit>,
    /// The relevance probe's co-files, heaviest first.
    lexical: Vec<String>,
    /// Answer-sized evidence cut from the best files, or one kind's own.
    excerpts: Vec<Excerpt>,
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
    /// The symbol lookup could not answer (a stale or missing graph, or a
    /// daemon that did not): the region pack falls back to the text search.
    symbols_failed: bool,
    /// Parts the region pack may fill; `0`: no pack.
    pack_limit: region_pack::Limit,
    /// The region pack, as complete as the window allowed.
    pack: Option<region_pack::Pack>,
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
    /// When the candidate searches were done: the end of the search stage.
    searched_at: Option<Instant>,
    /// When the worker returned, if it did before the brief was collected.
    worked_at: Option<Instant>,
}

impl Brief {
    /// The tier the relevance decision put the prompt in; `None` when it was
    /// not made, or when the model did not apply (no code graph).
    fn tier(&self) -> Option<Tier> {
        if self.lifted {
            return Some(Tier::Low);
        }
        self.relevance
            .as_ref()
            .filter(|verdict| verdict.basis != Basis::NoGraph)
            .map(|verdict| verdict.tier)
    }

    /// The packet names its own gaps: cut by the budget, unfinished, or with
    /// an unresolved note, and says so in its footer.
    fn partial(&self) -> bool {
        self.short() || !self.unresolved.is_empty()
    }

    /// The budget cut the chain short or it has not finished: the searches
    /// the receipt describes may not all have answered. An unresolved note of
    /// a kind op does not change what the searches covered, so a packet with
    /// one still carries its receipt and excerpts (and, being partial, no
    /// directive).
    fn short(&self) -> bool {
        self.cut || !self.finished
    }

    /// The receipt is shown: a confident (high-tier) brief of a prompt that
    /// did not name code, whose packet is complete.
    fn receipt_lines(&self) -> Vec<String> {
        if self.signal == Some(Signal::Strong) || self.tier() != Some(Tier::High) || self.short() {
            return Vec::new();
        }
        if !answer::receipt_enabled() {
            return Vec::new();
        }
        self.receipt
            .as_ref()
            .map(|receipt| receipt.lines(self.confident()))
            .unwrap_or_default()
    }

    /// The best chunk's excerpt: the first block cut from a file.
    fn top_chunk(&self) -> Option<&Excerpt> {
        self.excerpts
            .iter()
            .find(|excerpt| !excerpt.path.is_empty())
    }

    /// The ZERO step fires: a complete high-tier brief of a plain-language
    /// or weak prompt whose top chunk passes [`answer::zero_holds`] (the
    /// meaning search's best chunk, in the lexical probe's best co-file, with
    /// at least two probed keywords inside it).
    fn zero(&self) -> bool {
        if self.signal == Some(Signal::Strong) {
            return self.strong_zero && !self.short() && self.unresolved.is_empty();
        }
        let Some(rule) = self.zero_rule else {
            return false;
        };
        self.signal != Some(Signal::Strong)
            && self.tier() == Some(Tier::High)
            && !self.short()
            && self.unresolved.is_empty()
            && self.top_chunk().is_some_and(|top| {
                let rank = self.lexical.iter().position(|path| *path == top.path);
                answer::rule_holds(top, rank, rule)
            })
    }

    /// Whether the brief may keep `confidence: high`: a strong prompt's brief
    /// always does; a confident one only when the ZERO step fires.
    fn confident(&self) -> bool {
        if self.signal == Some(Signal::Strong) || self.tier() != Some(Tier::High) {
            return true;
        }
        self.zero()
    }

    /// The ranges the ONE-READ step names, best first: `(path, first, last,
    /// symbol)`. Empty for a cut packet or a brief without a relevance tier
    /// that resolved no definition.
    fn read_ranges(&self) -> Vec<(String, u64, u64, Option<String>)> {
        let strong = self.signal == Some(Signal::Strong);
        if self.short() || !(strong || matches!(self.tier(), Some(Tier::High | Tier::Low))) {
            return Vec::new();
        }
        self.reads
            .iter()
            .map(|read| {
                (
                    read.path.clone(),
                    read.first,
                    read.last,
                    read.symbol.clone(),
                )
            })
            .collect()
    }

    /// The `read:` and `also:` lines and the instruction under them.
    fn read_lines(&self) -> Vec<String> {
        let ranges = self.read_ranges();
        if ranges.is_empty() {
            return Vec::new();
        }
        let mut lines: Vec<String> = ranges
            .iter()
            .enumerate()
            .map(|(index, (path, first, last, symbol))| {
                let label = if index == 0 { "read" } else { "also" };
                let symbol = symbol
                    .as_deref()
                    .map_or_else(String::new, |name| format!(" — {}", clean(name)));
                format!("{label}: {}:{first}-{last}{symbol}", clean(path))
            })
            .collect();
        lines.push(READ_LINE.to_string());
        lines
    }

    fn has_excerpts(&self) -> bool {
        self.excerpt_counts().iter().any(|count| *count > 0)
    }

    /// The repository files the rendered block names, best first and without
    /// repeats: the region pack's blocks when the block is one, else the
    /// files, ranges, excerpts, definitions, tests and targets the lines list.
    fn injected_paths(&self, packed: bool) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        let mut add = |path: &str| {
            if !path.is_empty()
                && paths.len() < feedback::MAX_PATHS
                && !paths.iter().any(|held| held == path)
            {
                paths.push(path.to_string());
            }
        };
        if packed {
            if let Some(pack) = &self.pack {
                pack.paths().iter().for_each(|path| add(path));
            }
            return paths;
        }
        self.reads.iter().for_each(|read| add(&read.path));
        self.excerpts.iter().for_each(|excerpt| add(&excerpt.path));
        self.defined.iter().for_each(|hit| add(&hit.path));
        self.files
            .iter()
            .take(MAX_FILES)
            .for_each(|hit| add(&hit.path));
        self.tests.iter().for_each(|path| add(path));
        self.targets.iter().for_each(|path| add(path));
        paths
    }

    fn excerpt_counts(&self) -> [usize; EXCERPT_FILES] {
        let mut counts = [0; EXCERPT_FILES];
        // Excerpts are shown only for the ZERO step; otherwise they only
        // fix the ranges to read.
        if self.zero() {
            for (slot, excerpt) in counts.iter_mut().zip(&self.excerpts) {
                *slot = excerpt.lines.len();
            }
        }
        counts
    }

    /// The excerpt lines for the line counts in `shown`: one header per block
    /// that keeps a line.
    fn excerpt_block(&self, shown: Shown) -> Vec<String> {
        let mut lines = Vec::new();
        for (excerpt, count) in self.excerpts.iter().zip(shown.excerpt) {
            if count == 0 {
                continue;
            }
            lines.push(format!("answer {}:", clean(&excerpt.label)));
            lines.extend(
                excerpt
                    .lines
                    .iter()
                    .take(count)
                    .map(|line| format!("  {}", clean_n(line, MAX_ITEM_CHARS + 30))),
            );
        }
        lines
    }

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

/// End the search stage now, unless an earlier step already ended it.
fn mark_searched(brief: &mut Brief) {
    brief.searched_at.get_or_insert_with(Instant::now);
}

/// Whole milliseconds of `elapsed`, saturating.
fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The worker's two stages, from its marks: the search ran from `started`
/// to `searched`; ranking ran from there until the worker returned, or until
/// `collected` when the window cut it first. A brief that never searched has
/// neither, and one that closed after its searches did not rank.
fn work_stages(
    started: Instant,
    searched: Option<Instant>,
    worked: Option<Instant>,
    collected: Instant,
    ranked: bool,
) -> (Option<u64>, Option<u64>) {
    let Some(searched) = searched else {
        return (None, None);
    };
    let search = millis(searched.saturating_duration_since(started));
    let rank = ranked.then(|| {
        millis(
            worked
                .unwrap_or(collected)
                .min(collected)
                .saturating_duration_since(searched),
        )
    });
    (Some(search), rank)
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

/// Files the prompt names by path come first in `files`, each with its first
/// line: they are what was asked about, whatever else a search finds. A path
/// that does not read (partial, moved) is left to the search.
fn pin_named_files(
    plan: &Plan,
    evidence: &dyn Evidence,
    state: &Mutex<Brief>,
    deadline: Instant,
) -> usize {
    // A prompt that also names an identifier is about that identifier; its
    // path only selects the declaration.
    if plan.anchors.names().next().is_some() {
        return 0;
    }
    let mut pinned: Vec<RichHit> = Vec::new();
    for path in plan.anchors.paths() {
        if pixel_index::index::credential_path(Path::new(path)) || is_generated(path) {
            continue;
        }
        if let Ok(head) = evidence.line_at(path, 1, deadline) {
            pinned.push(RichHit {
                path: path.to_string(),
                line: 1,
                text: Some(head).filter(|head| !head.is_empty()),
            });
        }
    }
    let count = pinned.len();
    if pinned.is_empty() {
        return 0;
    }
    edit(state, |brief| {
        brief.files.retain(|file| {
            !pinned
                .iter()
                .any(|hit| hit.path == file.path && file.line == 1)
        });
        brief.files.splice(0..0, pinned);
        brief.searched = true;
        // The reads are one answered step, so a prompt that names only files
        // still gets its packet.
        brief.ops += 1;
        brief.answered += 1;
    });
    count
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
    let pinned = pin_named_files(plan, evidence, state, deadline);
    // Files named by path and all read need no word search: a stem such as
    // `scheduler` would only add prose mentions of it above nothing.
    let names_only_paths = plan.anchors.names().next().is_none();
    let all_pinned = names_only_paths && pinned > 0 && pinned == plan.anchors.paths().len();
    if let Some(term) = plan.anchors.search_term().filter(|_| !all_pinned)
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
                    // A stem taken from a path is a guess, not a name the
                    // prompt asked for: missing it is not a gap.
                    if !names_only_paths {
                        brief.unresolved.extend(note);
                    }
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
                brief.symbols_failed = true;
                brief
                    .unresolved
                    .push(format!("find-symbol {name}: {reason}"));
            }),
        }
    }
    // A code-shaped prompt's candidates are known here; a plain one's were
    // when its probes answered, and keep that mark.
    edit(state, mark_searched);
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
    if edit(state, |brief| brief.signal) == Some(Signal::Strong) {
        if answer::answer_enabled() && !edit(state, |brief| brief.cut) {
            strong_reads(plan, evidence, state, deadline);
        }
    } else {
        answer_evidence(evidence, state, deadline);
    }
    // The packet is complete here; the pack is an extra that may outlive the
    // window without making the packet partial.
    edit(state, |brief| brief.finished = true);
    pack_evidence(plan, evidence, state, deadline);
}

/// The region pack: the source the agent would read before answering, for
/// the symbols of the prompt (or, for plain words, the regions the chain
/// picked). Nothing is spent from the op budget — the pack is an extra, its
/// absence leaves the packet as it was — and it is published after each stage
/// so a window that ends first still renders complete blocks.
fn pack_evidence(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let mut brief = edit(state, |brief| brief.clone());
    // History rows answer a rationale question better than regions do.
    if !brief.pack_limit.on()
        || Instant::now() >= deadline
        || (!brief.history.is_empty() && plan.kind != QuestionKind::History)
    {
        return;
    }
    // A routed kind that reads no ranges of its own (a rationale whose
    // history is out of reach) still has its best files to read from.
    if brief.reads.is_empty()
        && brief.signal != Some(Signal::Strong)
        && brief.tier() == Some(Tier::High)
    {
        let terms: &[String] = brief.receipt.as_ref().map_or(&[], Receipt::terms);
        brief.reads = read_evidence(evidence, &brief, terms, deadline);
    }
    let names: Vec<String> = plan
        .anchors
        .names()
        .map(|name| name.rsplit("::").next().unwrap_or(name).to_string())
        .collect();
    let strong = brief.signal == Some(Signal::Strong);
    let first_rows = (strong && brief.searched && !plan.anchors.names().any(|n| n.contains("::")))
        .then(|| brief.files.clone())
        .filter(|rows| !rows.is_empty());
    let terms: Vec<String> = brief
        .receipt
        .as_ref()
        .map_or(&[][..], Receipt::terms)
        .to_vec();
    // Where a name the prompt's words spell may be found: the matched text of
    // the best files, the symbols of the meaning search's chunks, the symbols
    // of the regions picked.
    let hints: Vec<String> = brief
        .files
        .iter()
        .filter_map(|hit| hit.text.clone())
        .chain(brief.leads.iter().filter_map(|lead| lead.symbol.clone()))
        .chain(brief.reads.iter().filter_map(|read| read.symbol.clone()))
        .collect();
    // A tour, a review and a walkthrough are answered by the repository, the
    // change and the calls themselves; a question about a setting or about
    // tests by lines found by their shape. Neither names a symbol, so they
    // stand beside the symbols' blocks, or alone.
    let lead_symbols: Vec<(String, String)> = brief
        .leads
        .iter()
        .filter_map(|lead| {
            lead.symbol
                .clone()
                .map(|symbol| (symbol, lead.path.clone()))
        })
        .collect();
    let named: Vec<String> = plan.anchors.names().map(ToString::to_string).collect();
    let mut located = kinds::blocks(
        &KindInput {
            kind: plan.kind,
            typed: &plan.typed,
            names: &named,
            leads: &lead_symbols,
            hops: &brief.flow_hops,
            receipt: brief.receipt.as_ref(),
        },
        evidence,
        deadline,
    );
    let by_kind = !located.is_empty();
    // The regions closest to a few words are not the repository, nor the
    // change; and the words of a review ("commit", "latest", "tests") are
    // not a topic to look for tests of.
    let own = kinds::owns_the_pack(plan.kind);
    if !own {
        located.extend(super::locator::locate(
            &plan.typed,
            evidence,
            brief.receipt.as_ref(),
            deadline,
        ));
    }
    let input = region_pack::Input {
        names,
        phrases: region_pack::quoted_phrases(&plan.typed),
        hints: if own { Vec::new() } else { hints },
        terms,
        paths: plan
            .anchors
            .paths()
            .iter()
            .map(ToString::to_string)
            .collect(),
        defined: brief
            .defined
            .iter()
            .map(region_pack::DefSite::from)
            .collect(),
        graph_down: brief.symbols_failed,
        first_rows,
        callers: brief
            .callers
            .iter()
            .map(|caller| (caller.path.clone(), caller.line))
            .collect(),
        regions: !strong && !own,
        reads: brief
            .reads
            .iter()
            .filter(|_| !own)
            .map(|read| region_pack::ReadRegion {
                path: read.path.clone(),
                first: read.first,
                last: read.last,
                symbol: read.symbol.clone(),
            })
            .collect(),
        facts: brief
            .flow
            .iter()
            .map(|flow| format!("flow: {}", clean_n(flow, DEF_BODY_CHARS)))
            .collect(),
        wants_tests: brief.wants_tests,
        wants_docs: brief.wants_docs,
        locators: located,
        limit: brief.pack_limit,
        seen: brief.seen.clone(),
    };
    let mut publish = |pack: &region_pack::Pack| {
        edit(state, |brief| {
            brief.pack = Some(pack.clone());
            // A pack of the kind's own evidence is an answer even when the
            // probes behind the gate had none.
            if by_kind && brief.answered == 0 {
                brief.answered = 1;
            }
        });
    };
    if let Some(pack) = region_pack::build(&input, evidence, deadline, &mut publish) {
        publish(&pack);
    }
}

/// Whether the op budget and the window still allow an extra read. Unlike
/// [`spend`] it counts nothing and marks nothing: the answer excerpt is an
/// extra, its absence is not a gap in the evidence, and it is not an
/// `ops answered` entry of the coverage line.
fn has_budget(state: &Mutex<Brief>, deadline: Instant) -> bool {
    edit(state, |brief| brief.ops < MAX_OPS) && Instant::now() < deadline
}

/// Cut the answer-sized evidence of a confident brief: the routed kind's own
/// excerpt when its data is in, the generic one (signature, first doc line,
/// the matched region of the best files) otherwise. Nothing for a strong
/// prompt, a maybe, a partial packet or a rationale question.
fn answer_evidence(evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    if !answer::answer_enabled() {
        return;
    }
    let brief = edit(state, |brief| brief.clone());
    if brief.signal == Some(Signal::Strong)
        || !matches!(brief.tier(), Some(Tier::High | Tier::Low))
        || brief.cut
        || brief.kind == Some(QuestionKind::Rationale)
    {
        return;
    }
    // A maybe runs no kind route: its chunks only fix the ranges to read.
    let own = match brief.kind.filter(|_| brief.tier() == Some(Tier::High)) {
        Some(QuestionKind::Flow) => flow_excerpt(evidence, state, &brief, deadline),
        Some(QuestionKind::Tests) => tests_excerpt(evidence, &brief, deadline),
        Some(QuestionKind::Config) => {
            let terms = brief.receipt.as_ref().map_or(&[][..], Receipt::terms);
            let lines = answer::config_excerpt(&brief.files, terms);
            (!lines.is_empty()).then(|| Excerpt {
                label: "config".to_string(),
                lines,
                ..Excerpt::default()
            })
        }
        _ => None,
    };
    // The kind's own block leads and the generic excerpt of the best file
    // follows it: the dropped-first block is the generic one.
    let mut excerpts: Vec<Excerpt> = own.into_iter().collect();
    let room = EXCERPT_FILES - excerpts.len();
    excerpts.extend(generic_excerpts(evidence, &brief, room, deadline));
    let terms: &[String] = brief.receipt.as_ref().map_or(&[], Receipt::terms);
    let reads = read_evidence(evidence, &brief, terms, deadline);
    edit(state, |brief| {
        brief.excerpts = excerpts;
        brief.reads = reads;
    });
}

/// The ordered call chain of a flow question, one hop per line with the
/// site and first line of its definition. One extra lookup per name resolves them;
/// a hop that does not resolve to exactly one definition is named bare.
fn flow_excerpt(
    evidence: &dyn Evidence,
    state: &Mutex<Brief>,
    brief: &Brief,
    deadline: Instant,
) -> Option<Excerpt> {
    if brief.flow_hops.is_empty() {
        return caller_excerpt(evidence, brief, deadline);
    }
    let names = answer::pick_hops(&brief.flow_hops);
    if names.is_empty() || !has_budget(state, deadline) {
        return None;
    }
    let hops: Vec<Hop> = names
        .into_iter()
        .map(|name| {
            let site = evidence
                .symbols(&name, deadline)
                .ok()
                .filter(|hits| hits.len() == 1)
                .and_then(|hits| hits.into_iter().next())
                .filter(|hit| !pixel_index::index::credential_path(Path::new(&hit.path)));
            let head = site
                .as_ref()
                .and_then(|hit| evidence.line_at(&hit.path, hit.start_line, deadline).ok());
            Hop {
                name,
                site: site.map(|hit| (hit.path, hit.start_line)),
                head,
            }
        })
        .collect();
    let lines = answer::flow_excerpt(&hops);
    if lines.is_empty() {
        return None;
    }
    Some(Excerpt {
        label: "flow".to_string(),
        lines,
        ..Excerpt::default()
    })
}

/// A flow question with one endpoint: its direct callers, one per line with
/// the site and the first line of the calling declaration.
fn caller_excerpt(evidence: &dyn Evidence, brief: &Brief, deadline: Instant) -> Option<Excerpt> {
    let hops: Vec<Hop> = brief
        .callers
        .iter()
        .filter(|hit| !pixel_index::index::credential_path(Path::new(&hit.path)))
        .take(answer::MAX_HOPS)
        .map(|hit| Hop {
            name: hit.via.clone(),
            site: Some((hit.path.clone(), hit.line)),
            head: evidence.line_at(&hit.path, hit.line, deadline).ok(),
        })
        .collect();
    let lines = answer::flow_excerpt(&hops);
    (!lines.is_empty()).then(|| Excerpt {
        label: "callers".to_string(),
        lines,
        ..Excerpt::default()
    })
}

/// The tests of a tests question: each test function that mentions the
/// target, with its first assertion, read from the test files the `uses`
/// route found.
fn tests_excerpt(evidence: &dyn Evidence, brief: &Brief, deadline: Instant) -> Option<Excerpt> {
    let target = brief.defined.first()?.name.clone();
    let mut lines: Vec<String> = Vec::new();
    for path in brief.tests.iter().take(EXCERPT_FILES) {
        if lines.len() >= answer::MAX_TESTS || pixel_index::index::credential_path(Path::new(path))
        {
            continue;
        }
        let Ok(source) = evidence.lines_at(path, 1, answer::TEST_FILE_LINES, deadline) else {
            continue;
        };
        lines.extend(answer::tests_excerpt(
            path,
            &source,
            &target,
            answer::MAX_TESTS - lines.len(),
        ));
    }
    (!lines.is_empty()).then(|| Excerpt {
        label: "tests".to_string(),
        lines,
        ..Excerpt::default()
    })
}

/// A range of a file the brief tells the agent to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadRange {
    path: String,
    first: u64,
    last: u64,
    symbol: Option<String>,
}

/// Ranges one brief names.
const MAX_READS: usize = 3;
/// Lines of a file read to find its functions.
const MAX_READ_FILE_LINES: u64 = 20_000;

/// The best function-level chunk of each of the first three fused files,
/// scored by BM25 over the file's own chunks for the probed keywords (a tie
/// goes to the meaning search's chunk, then the lexical hit's); one range per
/// file, at most 60 lines. Tests and docs are skipped unless the question
/// asked for them.
fn read_evidence(
    evidence: &dyn Evidence,
    brief: &Brief,
    terms: &[String],
    deadline: Instant,
) -> Vec<ReadRange> {
    let tests_ok = brief.wants_tests || brief.kind == Some(QuestionKind::Tests);
    let mut paths: Vec<&str> = Vec::new();
    for hit in &brief.files {
        if paths.len() < MAX_READS
            && !paths.contains(&hit.path.as_str())
            && !answer::demoted(&hit.path, tests_ok, brief.wants_docs)
            && !pixel_index::index::credential_path(Path::new(&hit.path))
        {
            paths.push(&hit.path);
        }
    }
    let mut ranges = Vec::new();
    for path in paths {
        let Ok(lines) = evidence.lines_at(path, 1, MAX_READ_FILE_LINES, deadline) else {
            continue;
        };
        let mut chunks = answer::function_chunks(&lines);
        if !tests_ok {
            chunks.retain(|chunk| !answer::in_test_module(&lines, chunk.0));
        }
        let preferred: Vec<u64> = brief
            .leads
            .iter()
            .filter(|lead| lead.path == path)
            .map(|lead| u64::from(lead.start_line))
            .chain(
                brief
                    .files
                    .iter()
                    .filter(|hit| hit.path == path)
                    .map(|hit| hit.line),
            )
            .collect();
        let scores = answer::chunk_scores(&lines, &chunks, terms);
        let meaning: Vec<u64> = brief
            .leads
            .iter()
            .filter(|lead| lead.path == path)
            .map(|lead| u64::from(lead.start_line))
            .collect();
        let Some(best) = answer::best_chunk(&scores, &chunks, &preferred, &meaning) else {
            continue;
        };
        let (first, last) = answer::read_window(&lines, chunks[best], terms);
        ranges.push(ReadRange {
            path: path.to_string(),
            first,
            last,
            symbol: answer::chunk_symbol(&lines, (first, last), terms),
        });
    }
    ranges
}

/// The ZERO step of a code-shaped prompt: the only definition that resolved,
/// when it spans at most [`answer::STRONG_ZERO_LINES`] lines, becomes the
/// answer excerpt.
fn strong_answer(evidence: &dyn Evidence, state: &Mutex<Brief>, brief: &Brief, deadline: Instant) {
    let Some(def) = brief.defined.first().filter(|_| brief.unambiguous_def) else {
        return;
    };
    let span = def.end_line + 1 - def.start_line.min(def.end_line + 1);
    if !(1..=answer::STRONG_ZERO_LINES).contains(&span) {
        return;
    }
    let Ok(body) = evidence.lines_at(&def.path, def.start_line, def.end_line, deadline) else {
        return;
    };
    let lines = answer::body_excerpt(def.start_line, &body);
    if lines.is_empty() {
        return;
    }
    edit(state, |brief| {
        brief.strong_zero = true;
        brief.excerpts = vec![Excerpt {
            label: format!("{}:{}", def.path, def.start_line),
            path: def.path.clone(),
            lines,
            ..Excerpt::default()
        }];
    });
}

/// The ranges of a code-shaped prompt: the resolved definition (at most 60
/// lines), then up to two more from its callers' declarations, else from the
/// best chunk of the top fused files. Nothing when no definition resolved.
fn strong_reads(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    let brief = edit(state, |brief| brief.clone());
    let Some(def) = brief.defined.first() else {
        return;
    };
    if pixel_index::index::credential_path(Path::new(&def.path)) {
        return;
    }
    if answer::STRONG_ZERO && answer::zero_enabled() {
        strong_answer(evidence, state, &brief, deadline);
    }
    let mut ranges = vec![ReadRange {
        path: def.path.clone(),
        first: def.start_line,
        last: def
            .end_line
            .min(def.start_line + answer::READ_CAP as u64 - 1),
        symbol: Some(def.name.clone()),
    }];
    for caller in &brief.callers {
        if ranges.len() >= MAX_READS || pixel_index::index::credential_path(Path::new(&caller.path))
        {
            continue;
        }
        if ranges.iter().any(|read| read.path == caller.path) {
            continue;
        }
        let start = caller.line.saturating_sub(answer::WINDOW_BEFORE * 3).max(1);
        let Ok(window) = evidence.lines_at(
            &caller.path,
            start,
            caller.line + answer::READ_CAP as u64,
            deadline,
        ) else {
            continue;
        };
        let sig = answer::enclosing_start(start, &window, caller.line);
        let (first, last) = answer::declaration_range(start, &window, sig);
        ranges.push(ReadRange {
            path: caller.path.clone(),
            first,
            last,
            symbol: Some(caller.via.clone()),
        });
    }
    if ranges.len() < MAX_READS {
        let terms: Vec<String> = plan
            .typed
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|word| word.len() >= MIN_CONCEPT_WORD_CHARS)
            .filter(|word| !CONCEPT_STOPWORDS.contains(&word.to_lowercase().as_str()))
            .map(ToString::to_string)
            .collect();
        let mut rest = brief.clone();
        rest.files
            .retain(|hit| !ranges.iter().any(|read| read.path == hit.path));
        for read in read_evidence(evidence, &rest, &terms, deadline) {
            if ranges.len() < MAX_READS {
                ranges.push(read);
            }
        }
    }
    edit(state, |brief| brief.reads = ranges);
}

/// Where an excerpt may be cut from: a chunk of the meaning search, or the
/// declaration around a lexical hit.
struct Candidate {
    path: String,
    /// First line of the chunk, or the matched line of a lexical hit.
    line: u64,
    /// Last line of a meaning chunk; `0` for a lexical hit.
    end: u64,
    symbol: Option<String>,
    meaning: bool,
}

/// Files whose chunks compete for an excerpt: the head of the fused list.
const CHUNK_FILES: usize = 5;
/// Chunks scored, and chunks shown.
const MAX_CANDIDATES: usize = 10;
/// Lines read after the matched line of a lexical hit to cover its declaration.
const LEXICAL_CHUNK_LINES: u64 = 40;

fn excerpt_candidates(brief: &Brief) -> Vec<Candidate> {
    let tests_ok = brief.wants_tests || brief.kind == Some(QuestionKind::Tests);
    let docs_ok = brief.wants_docs;
    let mut top: Vec<&str> = Vec::new();
    for hit in &brief.files {
        if top.len() < CHUNK_FILES && !top.contains(&hit.path.as_str()) {
            top.push(&hit.path);
        }
    }
    let leads: Vec<&MeaningHit> = brief
        .leads
        .iter()
        .filter(|lead| top.is_empty() || top.contains(&lead.path.as_str()))
        .collect();
    let mut all: Vec<Candidate> = leads
        .iter()
        .map(|lead| Candidate {
            path: lead.path.clone(),
            line: u64::from(lead.start_line),
            end: u64::from(lead.end_line),
            symbol: lead.symbol.clone(),
            meaning: true,
        })
        .collect();
    for hit in brief
        .files
        .iter()
        .filter(|hit| top.contains(&hit.path.as_str()))
    {
        let covered = leads.iter().any(|lead| {
            lead.path == hit.path
                && (u64::from(lead.start_line)..=u64::from(lead.end_line)).contains(&hit.line)
        });
        if !covered && !all.iter().any(|c| c.path == hit.path && c.line == hit.line) {
            all.push(Candidate {
                path: hit.path.clone(),
                line: hit.line.max(1),
                end: 0,
                symbol: None,
                meaning: false,
            });
        }
    }
    all.retain(|candidate| {
        let packed = brief.def_body.is_some()
            && brief
                .defined
                .first()
                .is_some_and(|def| def.path == candidate.path);
        !(packed
            || pixel_index::index::credential_path(Path::new(&candidate.path))
            || (!tests_ok && answer::is_test_path(&candidate.path))
            || (!docs_ok && answer::is_docs_path(&candidate.path))
            || (!tests_ok
                && candidate
                    .symbol
                    .as_deref()
                    .is_some_and(answer::is_test_symbol)))
    });
    all.truncate(MAX_CANDIDATES);
    all
}

/// Lines of a Rust file read from the top so a `#[cfg(test)] mod` above the
/// match can be seen; longer prefixes are read as a window only.
const MAX_PREFIX_LINES: u64 = 20_000;

/// A chunk read and cut: where it is, how many keywords it holds, and the
/// excerpt of its densest region.
struct Chunk {
    candidate: Candidate,
    meaning_rank: usize,
    density: usize,
    lines: Vec<String>,
}

/// The chunks of the best files, cut where the question's keywords are
/// densest. Each candidate is scored by the reciprocal-rank fusion of its
/// meaning rank and its keyword-density rank; the best three are kept, one
/// per file except for the file of the best chunk.
fn generic_excerpts(
    evidence: &dyn Evidence,
    brief: &Brief,
    limit: usize,
    deadline: Instant,
) -> Vec<Excerpt> {
    let terms: &[String] = brief.receipt.as_ref().map_or(&[], Receipt::terms);
    let mut chunks: Vec<Chunk> = Vec::new();
    // Each retriever ranks its own candidates: a lexical hit is as good as
    // the meaning chunk of the same rank, not worse than all of them.
    let (mut meaning_seen, mut lexical_seen) = (0, 0);
    for candidate in excerpt_candidates(brief) {
        let rank = if candidate.meaning {
            meaning_seen += 1;
            meaning_seen - 1
        } else {
            lexical_seen += 1;
            lexical_seen - 1
        };
        let from_line = candidate.line;
        let mut last = if candidate.end > 0 {
            candidate.end.min(from_line + answer::MAX_CHUNK_LINES - 1)
        } else {
            from_line + LEXICAL_CHUNK_LINES
        };
        let start = from_line.saturating_sub(answer::WINDOW_BEFORE).max(1);
        // Far enough past the chunk to see the end of the declaration.
        let read_to = from_line + answer::READ_CAP as u64 + 2;
        let rust = candidate.path.ends_with(".rs") && read_to <= MAX_PREFIX_LINES;
        let from = if rust { 1 } else { start };
        let Ok(source) = evidence.lines_at(&candidate.path, from, read_to, deadline) else {
            continue;
        };
        if rust
            && answer::in_test_module(&source, from_line)
            && brief.kind != Some(QuestionKind::Tests)
        {
            continue;
        }
        let skip = usize::try_from(start - from).unwrap_or(0).min(source.len());
        let window = &source[skip..];
        let mut first = from_line;
        if !candidate.meaning {
            first = answer::enclosing_start(start, window, from_line);
            last = first + LEXICAL_CHUNK_LINES;
        }
        let body: Vec<&str> = window
            .iter()
            .skip(usize::try_from(first.saturating_sub(start)).unwrap_or(0))
            .take(usize::try_from(last.saturating_sub(first) + 1).unwrap_or(0))
            .map(String::as_str)
            .collect();
        if chunks
            .iter()
            .any(|seen| seen.candidate.path == candidate.path && seen.candidate.line == first)
        {
            continue;
        }
        let density = answer::distinct_terms(&body, terms);
        let (lines, _) = answer::chunk_excerpt(start, window, (first, last), terms);
        if lines.is_empty() {
            continue;
        }

        chunks.push(Chunk {
            candidate: Candidate {
                line: first,
                ..candidate
            },
            meaning_rank: rank,
            density,
            lines,
        });
    }
    // Rank by density, best first; ties keep the meaning order.
    let mut by_density: Vec<usize> = (0..chunks.len()).collect();
    by_density.sort_by_key(|&index| std::cmp::Reverse(chunks[index].density));
    let mut score = vec![0.0_f64; chunks.len()];
    for (density_rank, &index) in by_density.iter().enumerate() {
        score[index] = reciprocal_rank(chunks[index].meaning_rank) + reciprocal_rank(density_rank);
    }
    // The file the fused list ranks first speaks first: it is the right file
    // far more often than any other, so its best chunk leads.
    let lead_path = brief
        .files
        .first()
        .map(|hit| hit.path.as_str())
        .filter(|path| chunks.iter().any(|chunk| chunk.candidate.path == *path));
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by(|&a, &b| {
        let off = |index: usize| Some(chunks[index].candidate.path.as_str()) != lead_path;
        off(a)
            .cmp(&off(b))
            .then_with(|| score[b].total_cmp(&score[a]))
    });
    let top_path = order
        .first()
        .map(|&index| chunks[index].candidate.path.clone());
    let mut blocks: Vec<Excerpt> = Vec::new();
    for index in order {
        if blocks.len() >= limit {
            break;
        }
        let chunk = &chunks[index];
        let path = &chunk.candidate.path;
        if blocks.iter().any(|block| block.path == *path) && top_path.as_ref() != Some(path) {
            continue;
        }
        blocks.push(Excerpt {
            label: format!("{path}:{}", chunk.candidate.line),
            path: path.clone(),
            from_meaning: chunk.candidate.meaning,
            lines: chunk.lines.clone(),
            meaning_rank: chunk.candidate.meaning.then_some(chunk.meaning_rank),
            density: chunk.density,
        });
    }
    blocks
}

/// The extra ops of the routed kind: each spends the shared deadline like
/// the prefix did, and each lands in the brief under its own line.
fn run_kind(plan: &Plan, evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    match plan.kind {
        QuestionKind::Flow => flow_evidence(plan, evidence, state, deadline),
        QuestionKind::Tests => tests_evidence(evidence, state, deadline),
        QuestionKind::Config => config_evidence(plan, evidence, state, deadline),
        QuestionKind::Rationale | QuestionKind::History => {
            rationale_evidence(plan, evidence, state, deadline);
        }
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
        // Their evidence is the repository and the change themselves, read
        // by `pack_evidence`.
        QuestionKind::Overview | QuestionKind::Review => {}
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
            brief.flow_hops.clone_from(&hops);
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
    // The history below is read through the library; the command is only the
    // reader's next step, and a plugin provides it.
    if evidence.plugins().has("dig-history") {
        edit(state, |brief| {
            brief.next = Some(format!(
                "pixel dig-history --phrase {}",
                super::routes::q(&phrase)
            ));
        });
    }
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

/// The model that decided the tier, as the log names it.
fn gate_model_source() -> &'static str {
    super::gate_model::SOURCE
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
        tier: None,
        model: gate_model_source(),
        judge: None,
        kind: None,
        ops: 0,
        answered: 0,
        bytes: 0,
        receipt: false,
        answer: false,
        excerpt_bytes: 0,
        pack_bytes: 0,
        pack_parts: 0,
        pack_targets: 0,
        pack_located: 0,
        fallback: false,
        elapsed_ms: 0,
        cut: false,
        deadline_ms: None,
        stages: Stages::default(),
        route: None,
        daemon: None,
        probes: Vec::new(),
        typed: decision_log::logged_typed(typed),
        sha256: decision_log::sha256_hex(typed),
    }
}

/// Starts a daemon in the background for a prompt that is going to be
/// briefed; the kick it returns is awaited, briefly, when the brief is
/// collected, so a hook about to exit has let the launch happen.
pub(crate) type Starter = Box<dyn FnOnce() -> autostart::Kicked + Send>;

/// How the files of the session are collected for a prompt, by a deadline.
pub(crate) type ContextSource = Box<dyn FnOnce(Instant) -> SessionPaths + Send>;

/// Environment opt-out of the session's files: `0`, `false` or `off`.
pub(crate) const CONTEXT_ENV: &str = "PIXEL_BRIEF_CONTEXT";

/// What a host tells the brief about the session a prompt belongs to.
#[derive(Clone, Debug, Default)]
pub(crate) struct Session {
    /// The host's id for the session, when it names one: the key of what the
    /// session has been sent (see `sent`).
    pub(crate) id: Option<String>,
    /// The host's transcript of the session, when it names one.
    pub(crate) transcript: Option<PathBuf>,
}

/// What a brief does around its chain besides running it.
#[derive(Default)]
pub(crate) struct Around {
    /// Where the decision is recorded, when it is.
    pub(crate) log: Option<PathBuf>,
    /// The repository a central mirror records for, when the central log is on.
    pub(crate) central: Option<PathBuf>,
    /// How a daemon is started for the prompt, when one may be.
    pub(crate) starter: Option<Starter>,
    /// What the region pack may be; the default leaves the packet as it was.
    pub(crate) pack: region_pack::Limit,
    /// The files the session is working on, when the host's hook has a session.
    pub(crate) context: Option<ContextSource>,
    /// Ranges an earlier pack sent to this session: the pack leaves them out.
    pub(crate) seen: Seen,
    /// Where the ranges this pack sends are remembered: the repository and
    /// the session.
    pub(crate) sent: Option<(PathBuf, String)>,
    /// Where the brief's probes are looked up and run, when they may be.
    pub(crate) probes: Option<ProbeSetup>,
    /// Whether a French prompt is searched and gated on the English words
    /// code uses (`french::english`) instead of its own.
    pub(crate) french: bool,
}

/// The repository whose brief is probed and the plugin host that offers the
/// probes (the user's own plus the repository's trusted ones).
pub(crate) struct ProbeSetup {
    pub(crate) host: pixel_plugin::Host,
    pub(crate) root: PathBuf,
    /// Characters the merged brief may hold: the hook parts the host takes,
    /// whatever the tier shrank the region pack to (a maybe's pack is one
    /// part, but a probe that answers it may fill all of them).
    pub(crate) cap: usize,
    /// What the probes are given at least after the chain: [`PROBE_FLOOR`] for a
    /// hook, nothing for the `brief` command (OpenCode kills it at 1.2 s).
    pub(crate) floor: Duration,
    /// Bytes the merged brief may hold when the host drops a longer one (the
    /// `brief` command's OpenCode extension drops above 4 000).
    pub(crate) max_bytes: Option<usize>,
}

impl Around {
    /// Record the decision at `path` and start nothing.
    #[cfg(test)]
    pub(crate) fn logging(path: PathBuf) -> Self {
        Self {
            log: Some(path),
            central: None,
            starter: None,
            ..Self::default()
        }
    }

    /// Record the decision at `path` and mirror it to the central store of
    /// `central_root`, when both are given.
    #[cfg(test)]
    pub(crate) fn logging_with_central(path: PathBuf, central_root: Option<PathBuf>) -> Self {
        Self {
            central: central_root,
            ..Self::logging(path)
        }
    }
}

/// What a probe is always given at least once the chain is done, even when
/// the chain used the window up: a loaded machine slows the chain and the
/// probes alike, and a probe that never gets to run is a brief without it.
const PROBE_FLOOR: Duration = Duration::from_millis(1_200);
/// No probe outlives this after the prompt arrived: under Pi's six-second hook
/// decision limit, with room to print, and under what the parts after the first
/// wait for part 1.
const PROBE_CEILING: Duration = Duration::from_millis(5_000);
/// What the one-part `brief` command may print once probes added to it: a
/// caller that did not ask for more parts (`--parts`) takes one 4 000-byte
/// message.
const COMMAND_MAX_BYTES: usize = 3_990;

/// When the probes must be done: the end of the brief's window, or `now` plus
/// the floor if the chain left less than that, and never past the ceiling.
fn probe_deadline(started: Instant, window_end: Instant, now: Instant, floor: Duration) -> Instant {
    window_end.max(now + floor).min(started + PROBE_CEILING)
}

/// A brief on its way: the worker is running, the deadline is fixed.
pub(crate) struct Pending {
    state: Arc<Mutex<Brief>>,
    done: Receiver<()>,
    deadline: Instant,
    started: Instant,
    /// Where the decision is recorded, when it is.
    log: Option<PathBuf>,
    /// The repository the decision is mirrored to the central store for,
    /// when the central log is on: it gives the record its `root`.
    central: Option<PathBuf>,
    /// The daemon start this prompt gave, when it gave one.
    kicked: Option<autostart::Kicked>,
    /// The text the brief judged: the typed prompt, its last paragraph, or the
    /// excerpt of the paste for a prompt that is only a pasted block.
    task: String,
    signal: Signal,
    /// Where the ranges the pack sends are remembered for the session.
    sent: Option<(PathBuf, String)>,
    /// Where the probes are run, when they are.
    probes: Option<ProbeSetup>,
}

/// A finished brief: the block, when there is one, and what was decided.
pub(crate) struct Finished {
    pub(crate) text: Option<String>,
    pub(crate) record: Record,
    /// The repository files the block names, best first: what the agent was
    /// handed to read, for `feedback`. Empty when there is no block.
    pub(crate) paths: Vec<String>,
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

    fn probe_stage(
        &self,
        brief: &Brief,
        text: Option<String>,
    ) -> (Option<String>, Vec<probes::ProbeRun>) {
        let Some(setup) = &self.probes else {
            return (text, Vec::new());
        };
        let gate_closed = matches!(brief.admission, Admission::Closed(_));
        if matches!(brief.admission, Admission::Denied(_)) || (brief.silenced && !gate_closed) {
            return (text, Vec::new());
        }
        let kind = brief.kind.map(QuestionKind::as_str);
        let stage = probes::Stage {
            host: &setup.host,
            root: &setup.root,
            prompt: &self.task,
            kind,
            gate_closed,
            deadline: probe_deadline(self.started, self.deadline, Instant::now(), setup.floor),
            cap: setup.cap,
            max_bytes: setup.max_bytes,
        };
        match probes::apply(&stage, text.as_deref().unwrap_or("")) {
            (Some(merged), runs) => (Some(merged), runs),
            (None, runs) => (text, runs),
        }
    }

    /// [`Pending::finish`], and the decision it recorded.
    pub(crate) fn finish_with_record(self) -> Finished {
        let timed_out = matches!(
            self.done
                .recv_timeout(self.deadline.saturating_duration_since(Instant::now())),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let collected = Instant::now();
        let mut brief = edit(&self.state, |brief| brief.clone());
        // The window ended before the worker did: what is rendered is what
        // had arrived, and `fit` says so instead of staying silent.
        brief.cut |= timed_out;
        let (search_ms, rank_ms) = work_stages(
            self.started,
            brief.searched_at,
            brief.worked_at,
            collected,
            !brief.silenced,
        );
        let render_started = Instant::now();
        let (text, stats) = match fit(&brief) {
            Some((text, stats)) => (Some(text), stats),
            None => (None, AnswerStats::default()),
        };
        let render_ms = millis(render_started.elapsed());
        let probe_started = Instant::now();
        let (text, probe_runs) = self.probe_stage(&brief, text);
        let probe_ms = (!probe_runs.is_empty()).then(|| millis(probe_started.elapsed()));
        let recovered = matches!(brief.admission, Admission::Closed(_)) && text.is_some();
        let elapsed_ms = millis(self.started.elapsed());
        // The daemon start has had the whole brief to launch; a hook about to
        // exit sees it through, for at most the hook bound.
        let daemon = self
            .kicked
            .as_ref()
            .and_then(|kicked| kicked.wait(autostart::HOOK_BOUND))
            .map(autostart::Outcome::as_str);
        let record = Record {
            ts_ms: pixel_task::now_ms(),
            signal: Some(self.signal.as_str()),
            gate: brief.admission.label(),
            enforced: gate_enforced(self.signal, brief.silenced, ENFORCE_GATE_ON_WEAK)
                && !recovered,
            reason: brief.admission.reason().map(str::to_string),
            tier: brief.tier().map(Tier::as_str),
            model: gate_model_source(),
            score: brief.relevance.as_ref().and_then(|verdict| verdict.score),
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
            receipt: stats.receipt,
            answer: stats.answer,
            excerpt_bytes: stats.excerpt_bytes,
            pack_bytes: stats.pack_bytes,
            pack_parts: stats.pack_parts,
            pack_targets: stats.pack_targets,
            pack_located: stats.pack_located,
            fallback: stats.fallback,
            elapsed_ms,
            cut: brief.cut,
            deadline_ms: Some(millis(
                self.deadline.saturating_duration_since(self.started),
            )),
            stages: Stages {
                search_ms,
                rank_ms,
                render_ms: Some(render_ms),
                probe_ms,
            },
            route: brief.route,
            daemon,
            probes: probe_runs,
            typed: decision_log::logged_typed(&self.task),
            sha256: decision_log::sha256_hex(&self.task),
        };
        if let Some(path) = &self.log {
            // Best effort: a log that cannot be written never costs the prompt.
            let _ = decision_log::append(path, &record.line(), decision_log::MAX_LINES);
            if let Some(root) = &self.central {
                // The same record, mirrored best effort to the per-user store.
                let _ = decision_log::append_central(root, &record.line());
            }
        }
        let paths = if text.is_some() {
            brief.injected_paths(stats.pack_bytes > 0)
        } else {
            Vec::new()
        };
        // What the pack just sent is not sent to this session again.
        if let (Some((root, session)), Some(pack), true) =
            (&self.sent, &brief.pack, stats.pack_bytes > 0)
        {
            let _ = sent::record(root, session, &pack.ranges(), pixel_task::now_ms());
        }
        Finished {
            text,
            record,
            paths,
        }
    }
}

/// Start the brief for `prompt` of a hook session in `root`, or decline: a
///
/// `parts` is how many hooks deliver the brief (`brief_parts`, or 1 for a host
/// with one hook): the region pack may fill that many. `session` is what the
/// host says of the session: with it the files the session is already working
/// on (its transcript's recent tool calls, the dirty files of the working
/// tree) boost the leads, and let a short follow-up through the gate.
pub(crate) fn start_in(prompt: &str, root: &Path, parts: u8, session: Session) -> Option<Pending> {
    try_start_as(prompt, root, false, parts, Some(session)).ok()
}

/// [`start_in`] for the `brief` command, which a host without a prompt hook
/// (OpenCode) runs and which it drops above 4 000 bytes: a region pack stays
/// under that unless `PIXEL_BRIEF_PARTS` says the host takes more.
pub(crate) fn start_for_command(prompt: &str, root: &Path, parts: u8) -> Option<Pending> {
    try_start_for_command(prompt, root, parts).ok()
}

/// [`start_for_command`], saying why it declined. `parts` is the hook-sized
/// pieces the caller takes (`pixel brief --parts N`, 1 by default): above 1
/// the pack and the probes get the same budget as an N-part prompt hook.
pub(crate) fn try_start_for_command(
    prompt: &str,
    root: &Path,
    parts: u8,
) -> Result<Pending, Declined> {
    try_start_as(prompt, root, true, parts.max(1), None)
}

fn try_start_as(
    prompt: &str,
    root: &Path,
    command: bool,
    parts: u8,
    session: Option<Session>,
) -> Result<Pending, Declined> {
    let log = decision_log::path_for(root);
    let central = log
        .is_some()
        .then(|| decision_log::central_log_path(root).map(|_| root.to_path_buf()))
        .flatten();
    let gate = Gate::read(root);
    let root = root.to_path_buf();
    let for_start = root.clone();
    let for_context = root.clone();
    let for_index = root.clone();
    let dedupe = sent::enabled();
    let session_id = session.as_ref().and_then(|session| session.id.clone());
    // What this session was sent by earlier prompts, and where this prompt's
    // pack is remembered.
    let seen = session_id
        .as_deref()
        .filter(|_| dedupe)
        .map(|id| sent::load(&root, id, pixel_task::now_ms()))
        .unwrap_or_default();
    let sent_to = session_id.filter(|_| dedupe).map(|id| (root.clone(), id));
    let context: Option<ContextSource> = session
        .filter(|_| decision_log::enabled(std::env::var(CONTEXT_ENV).ok().as_deref()))
        .map(|session| -> ContextSource {
            Box::new(move |deadline| {
                session_paths::collect(&for_context, session.transcript.as_deref(), deadline)
            })
        });
    let pack = region_pack::limit_from_env(command, parts);
    let window = window_for(&root, command);
    let probes_setup = probes::enabled(std::env::var(probes::PROBES_ENV).ok().as_deref())
        .then(|| crate::plugin_cmd::brief_host(&root))
        .flatten()
        .map(|host| ProbeSetup {
            host,
            root: root.clone(),
            cap: pack.parts.max(1) * pack.chars,
            floor: if command { Duration::ZERO } else { PROBE_FLOOR },
            max_bytes: (command && pack.parts <= 1).then_some(COMMAND_MAX_BYTES),
        });
    let result = try_start_with(
        prompt,
        gate,
        window,
        move |deadline| super::evidence::open(&root, deadline),
        super::intent::judge,
        relevance::judge,
        Around {
            log,
            central,
            starter: Some(Box::new(move || autostart::start_for_prompt(&for_start))),
            pack,
            context,
            seen,
            sent: sent_to,
            probes: probes_setup,
            french: super::french::enabled(
                std::env::var(super::french::FRENCH_ENV).ok().as_deref(),
            ),
        },
    );
    let auto_start = crate::config_cmd::feature_enabled(
        Some(&for_index),
        crate::config_cmd::DAEMON_AUTO_START_FEATURE,
        crate::config_cmd::DAEMON_AUTO_START_ENV,
    );
    kick_index(
        result.as_ref().err(),
        prompt,
        &for_index,
        auto_start,
        lazy_index,
    );
    result
}

fn kick_index(
    declined: Option<&Declined>,
    prompt: &str,
    root: &Path,
    auto_start: bool,
    launch: impl FnOnce(&Path),
) {
    if auto_start
        && matches!(declined, Some(Declined::Unindexed))
        && !crate::prompt_continuation::is_trivial_continuation(prompt)
        && brief_signal(prompt).is_some()
    {
        launch(root);
    }
}

fn lazy_index(root: &Path) {
    let args = [
        std::ffi::OsString::from("prepare-repo"),
        root.as_os_str().to_os_string(),
    ];
    let _ = crate::spawn_detached(&args);
}

/// [`start_in`] with its gate, window, evidence source and intent judge given and
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
    try_start_with(
        prompt,
        gate,
        window,
        open,
        judge,
        relevance::judge,
        Around::default(),
    )
    .ok()
}

/// [`try_start_as`] with its gate, window, evidence source, intent judge, tier
/// model and decision log given. The judge runs on a weakly code-shaped or
/// plain prompt, beside the relevance probe and the meaning search; on a
/// weak prompt a denying verdict ends the brief before any evidence op, and
/// no verdict means the heuristic plan.
pub(crate) fn try_start_with<F, J>(
    prompt: &str,
    gate: Gate,
    window: Duration,
    open: F,
    judge: J,
    model: Model,
    around: Around,
) -> Result<Pending, Declined>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
    J: Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
{
    let Around {
        log,
        central,
        starter,
        pack,
        context,
        seen,
        sent,
        probes,
        french,
    } = around;
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
            if let Some(central) = &central {
                let _ = decision_log::append_central(central, &record.line());
            }
        }
        return Err(Declined::NotAboutCode);
    };
    // The prompt is going to be briefed: let a daemon come up for the next
    // one, whatever route this one takes.
    let kicked = starter.map(|start| start());
    let started = Instant::now();
    let deadline = started + window;
    let state = Arc::new(Mutex::new(Brief {
        signal: Some(signal),
        pack_limit: pack,
        seen,
        ..Brief::default()
    }));
    let (finished, done) = mpsc::channel();
    let worker = Arc::clone(&state);
    // The decision log and the probes keep the text as typed; the searches,
    // the gate and the plan read a French prompt in the English words code
    // uses.
    let task = typed.clone();
    let typed = if french {
        super::french::english(&typed).unwrap_or(typed)
    } else {
        typed
    };
    // The session's files are collected beside the worker, inside their own
    // short window, and only a prompt that is going to be briefed pays for it.
    let context_deadline = (started + session_paths::CONTEXT_WINDOW).min(deadline);
    let context = context.and_then(|source| {
        let (send, receive) = mpsc::channel();
        std::thread::Builder::new()
            .name("pixel-brief-context".into())
            .spawn(move || {
                let _ = send.send(source(context_deadline));
            })
            .ok()
            .map(|_| (receive, context_deadline))
    });
    std::thread::Builder::new()
        .name("pixel-brief".into())
        .spawn(move || {
            let job = Job {
                typed: &typed,
                signal,
                deadline,
                state: &worker,
                finished: &finished,
                judge: &judge,
                model,
                context: &context,
            };
            work(&job, open);
            edit(&worker, |brief| brief.worked_at = Some(Instant::now()));
            let _ = finished.send(());
        })
        .map_err(|_| Declined::NoWorker)?;
    Ok(Pending {
        state,
        done,
        deadline,
        started,
        log,
        central,
        kicked,
        task,
        signal,
        sent,
        probes,
    })
}

/// What the worker of one brief is given.
struct Job<'a, J> {
    typed: &'a str,
    signal: Signal,
    deadline: Instant,
    state: &'a Mutex<Brief>,
    finished: &'a Sender<()>,
    judge: &'a J,
    model: Model,
    /// The session's files on their way, and when they stop being waited for.
    context: &'a Option<(Receiver<SessionPaths>, Instant)>,
}

/// The worker of one brief: plan, ask, run.
fn work<F, J>(job: &Job<J>, open: F)
where
    F: FnOnce(Instant) -> Box<dyn Evidence>,
    J: Fn(&str, Instant) -> Option<Verdict> + Sync,
{
    let Job {
        typed,
        signal,
        deadline,
        state,
        finished,
        judge,
        model,
        context,
    } = *job;
    if signal == Signal::Strong {
        let plan = Plan::from_typed(typed, has_change_intent(typed), None);
        let evidence = open(deadline);
        edit(state, |brief| brief.route = Some(evidence.route()));
        run(&plan, evidence.as_ref(), state, deadline);
        return;
    }
    let evidence = open(deadline);
    edit(state, |brief| brief.route = Some(evidence.route()));
    let kind = QuestionKind::heuristic(typed, &Anchors::from_text(typed));
    // The session's files: they boost the leads, and a short follow-up that
    // the words alone cannot place is let through on them.
    let session = context
        .as_ref()
        .and_then(|(receive, until)| {
            receive
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .ok()
        })
        .unwrap_or_default()
        .ranked();
    let lift = !session.is_empty() && is_follow_up(typed);
    let mut settled = |got: &Gathered| match refusal_lifting(
        signal,
        got,
        false,
        ENFORCE_GATE_ON_WEAK,
        kind,
        lift,
    ) {
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
        model,
        state,
        deadline,
        &mut settled,
    );
    edit(state, mark_searched);
    if edit(state, |brief| brief.silenced) {
        return;
    }
    if let Some(admission) = refusal_lifting(signal, &got, true, ENFORCE_GATE_ON_WEAK, kind, lift) {
        close(state, &got, admission);
        return;
    }
    let (change_intent, label) = match got.judged.as_ref().and_then(Option::as_ref) {
        Some(verdict) => (verdict.change_intent(), Some(verdict.label.clone())),
        None => (has_change_intent(typed), None),
    };
    let plan = Plan::from_typed(typed, change_intent, label.as_deref());
    fold_with(state, &plan, &got, &session, lift);
    // A maybe is a short list of files and nothing else: no kind route, no
    // definition read, no fallback search.
    if edit(state, |brief| brief.tier()) == Some(Tier::Low) {
        answer_evidence(evidence.as_ref(), state, deadline);
        edit(state, |brief| {
            brief.finished = true;
            // A maybe gets at most one part of regions, as a lead.
            brief.pack_limit.parts = brief.pack_limit.parts.min(1);
        });
        pack_evidence(&plan, evidence.as_ref(), state, deadline);
        return;
    }
    let plan = seeded(plan, state);
    run(&plan, evidence.as_ref(), state, deadline);
}

/// A flow or tests question in plain language names no symbol, so the kind's
/// route has nothing to resolve: the symbol of the meaning search's best
/// non-test chunk stands in as its anchor, for a confident brief only.
fn seeded(mut plan: Plan, state: &Mutex<Brief>) -> Plan {
    if !matches!(plan.kind, QuestionKind::Flow | QuestionKind::Tests)
        || plan.anchors.names().next().is_some()
    {
        return plan;
    }
    let seed = edit(state, |brief| {
        (brief.tier() == Some(Tier::High) && answer::answer_enabled())
            .then(|| {
                brief
                    .leads
                    .iter()
                    .filter(|lead| !answer::is_test_path(&lead.path))
                    .find_map(|lead| lead.symbol.clone())
            })
            .flatten()
    });
    if let Some(symbol) = seed
        .filter(|name| name.len() >= 3 && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_'))
    {
        plan.anchors.0.push(symbol);
    }
    plan
}

/// What the probes of a weak or plain prompt have answered so far.
#[derive(Default)]
struct Gathered {
    relevance: Option<Result<RelevanceAnswer, String>>,
    /// The decision scored from an answered relevance probe.
    scored: Option<relevance::Verdict>,
    meaning: Option<Result<Vec<MeaningHit>, String>>,
    /// The plain-word leads, asked only when the meaning search failed.
    words: Option<Result<Vec<MeaningHit>, String>>,
    /// `Some(None)`: the judge ran and had no verdict.
    judged: Option<Option<Verdict>>,
}

/// One probe's answer on its way back to the worker.
enum Reply {
    Relevance(Result<RelevanceAnswer, String>),
    Meaning(Result<Vec<MeaningHit>, String>),
    Words(Result<Vec<MeaningHit>, String>),
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
    model: Model,
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
                let leads = evidence.meaning(typed, MEANING_LIMIT, deadline);
                let fallback = leads.is_err();
                let _ = send.send(Reply::Meaning(leads));
                if fallback {
                    let _ = send.send(Reply::Words(evidence.word_leads(
                        typed,
                        MEANING_LIMIT,
                        deadline,
                    )));
                }
            });
        }
        scope.spawn(move || {
            let _ = send.send(Reply::Judge(judge(typed, deadline)));
        });
        while let Ok(reply) =
            receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            match reply {
                Reply::Relevance(answer) => got.relevance = Some(answer),
                Reply::Meaning(leads) => got.meaning = Some(leads),
                Reply::Words(leads) => got.words = Some(leads),
                Reply::Judge(verdict) => got.judged = Some(verdict),
            }
            score(&mut got, typed, model);
            if settled(&got) {
                break;
            }
        }
    });
    got
}

/// Put the relevance answer through `model`, once; a no-op until the
/// relevance probe has answered.
fn score(got: &mut Gathered, typed: &str, model: Model) {
    if got.scored.is_some() {
        return;
    }
    let Some(Ok(answer)) = &got.relevance else {
        return;
    };
    let verdict = model(&GateInput {
        relevance: &answer.input,
        typed,
    });
    got.scored = Some(verdict);
}

/// Why `signal`'s prompt gets no brief, if it does not. `complete`: no more
/// answers are coming, so a probe that never answered counts as "cannot tell".
/// `lift`: a short follow-up of a session that is working on files, which the
/// relevance gate does not close (see [`is_follow_up`]): not by scoring it off
/// topic, and not by a probe that failed or never answered, which says nothing
/// about the prompt.
/// Plain language must be shown on topic by the repository, the intent judge
/// having no say in it; a weak prompt is refused by the judge, and by the
/// relevance decision when `enforce_weak`.
fn refusal_lifting(
    signal: Signal,
    got: &Gathered,
    complete: bool,
    enforce_weak: bool,
    kind: QuestionKind,
    lift: bool,
) -> Option<Admission> {
    let verdict = got.judged.as_ref().and_then(Option::as_ref);
    match signal {
        Signal::Strong => None,
        Signal::Weak => {
            if let Some(verdict) = verdict.filter(|verdict| verdict.denies_brief()) {
                return Some(Admission::Denied(judge_reason(verdict)));
            }
            // Without a code graph the model does not apply: a weak prompt
            // keeps the behaviour it had before the gate.
            got.scored
                .as_ref()
                .filter(|scored| {
                    enforce_weak
                        && scored.tier == Tier::Off
                        && scored.basis != Basis::NoGraph
                        && !lift
                        && !admitted_by_kind(kind, Some(scored))
                        && !admitted_by_coverage(scored)
                })
                .map(|scored| Admission::Closed(off_topic_reason(scored)))
        }
        Signal::Prose => match (&got.relevance, &got.scored) {
            // A tour or a review is about the repository whatever words it
            // uses, and whether or not the probe could be asked.
            _ if admitted_by_kind(kind, got.scored.as_ref()) => None,
            // The session's files place a short follow-up whatever the probe
            // said or failed to say: `fold_with` lifts it onto them.
            _ if lift => None,
            (Some(Err(reason)), _) => Some(Admission::Closed(format!("relevance: {reason}"))),
            (_, Some(scored)) if scored.tier == Tier::Off && !admitted_by_coverage(scored) => {
                Some(Admission::Closed(off_topic_reason(scored)))
            }
            (None, _) if complete => Some(Admission::Closed(
                "relevance: no answer before the deadline".to_string(),
            )),
            _ => None,
        },
    }
}

/// [`refusal_lifting`] for a prompt of no session: nothing lifts it.
#[cfg(test)]
fn refusal(
    signal: Signal,
    got: &Gathered,
    complete: bool,
    enforce_weak: bool,
    kind: QuestionKind,
) -> Option<Admission> {
    refusal_lifting(signal, got, complete, enforce_weak, kind, false)
}

/// Keywords one file must hold, at least, for a walkthrough to be about the
/// repository: the prompt shares two words with a file the graph knows.
const FLOW_MIN_SHARED: usize = 2;

/// Whether the kind of the prompt lets it through a gate that read it as off
/// topic. A tour and a review of a change are about the repository by their
/// phrasing, so they need no vocabulary; a walkthrough asks about a thing,
/// and must share words with a file the graph knows by its symbols.
fn admitted_by_kind(kind: QuestionKind, scored: Option<&relevance::Verdict>) -> bool {
    match kind {
        QuestionKind::Overview | QuestionKind::Review => true,
        QuestionKind::Flow | QuestionKind::History => scored.is_some_and(|scored| {
            scored.features.shared >= FLOW_MIN_SHARED && scored.features.structural_files > 0
        }),
        _ => false,
    }
}

/// A prompt the model read as off topic is still about the repository when
/// one structural co-file holds half or more of its informative keywords:
/// the words name something the codebase knows by name or symbol — the bar
/// a walkthrough already passes, made of coverage on top of it.
fn admitted_by_coverage(scored: &relevance::Verdict) -> bool {
    scored.features.shared >= FLOW_MIN_SHARED
        && scored.features.shared * 2 >= scored.features.informative
        && scored.features.structural_files > 0
}

fn judge_reason(verdict: &Verdict) -> String {
    format!("judge: {} ({:.2})", verdict.label, verdict.confidence)
}

fn off_topic_reason(scored: &relevance::Verdict) -> String {
    match (scored.basis, scored.score) {
        (Basis::Model, Some(score)) => format!(
            "off topic: score {score:.2}, {}/{} key terms in the best file",
            scored.features.shared, scored.features.informative
        ),
        (Basis::NoKeywords, _) => "off topic: no keyword to weigh".to_string(),
        _ => "no code graph: the model does not apply".to_string(),
    }
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
        mark_searched(brief);
        note(brief, got);
        brief.admission = admission;
        brief.silenced = true;
    });
}

/// Fold what the probes found into the brief: the decision, and the fused
/// leads as its first files, so the literal and concept searches only run
/// when they found nothing.
fn fold_with(state: &Mutex<Brief>, plan: &Plan, got: &Gathered, session: &[String], lift: bool) {
    let leads: &[MeaningHit] = match &got.meaning {
        Some(Ok(leads)) => leads,
        _ => &[],
    };
    let lines: &[RichHit] = match &got.relevance {
        Some(Ok(answer)) => &answer.lines,
        _ => &[],
    };
    // Without the meaning search's leads, the plain-word leads rank in their
    // place; they reach the fusion only, never `brief.leads`.
    let ranked: &[MeaningHit] = match (&got.meaning, &got.words) {
        (Some(Ok(_)), _) => leads,
        (_, Some(Ok(words))) => words,
        _ => leads,
    };
    // The gate would have closed this prompt and the session's files stand in
    // for what its words cannot say: they are the brief's files, and the
    // words' leads (off topic by the gate's own verdict) are left out.
    let lifted = lift
        && !session.is_empty()
        && got
            .scored
            .as_ref()
            .is_none_or(|scored| scored.tier == Tier::Off);
    let (leads, ranked, lines): (&[MeaningHit], &[MeaningHit], &[RichHit]) = if lifted {
        (&[], &[], &[])
    } else {
        (leads, ranked, lines)
    };
    let fused = RichFound {
        hits: fuse_with(ranked, lines, session, lifted),
        capped: false,
    };
    let admit_json = plan.kind == QuestionKind::Config;
    edit(state, |brief| {
        note(brief, got);
        brief.lifted = lifted;
        brief.admission = match &got.scored {
            _ if lifted => Admission::Context,
            Some(scored) if scored.basis == Basis::NoGraph => Admission::Unjudged,
            Some(scored) if scored.tier != Tier::Off => Admission::Open,
            Some(scored) if admitted_by_coverage(scored) => Admission::Open,
            scored if admitted_by_kind(plan.kind, scored.as_ref()) => Admission::Kind(plan.kind),
            Some(scored) => Admission::Closed(off_topic_reason(scored)),
            None => Admission::Unjudged,
        };
        let answers = usize::from(matches!(got.relevance, Some(Ok(_))))
            + usize::from(matches!(got.meaning, Some(Ok(_))));
        if answers > 0 {
            brief.answered += answers;
            brief.searched = true;
        } else if lifted {
            // No probe could be asked, but the session's files are the brief:
            // without this `fit` drops it as a brief of failures.
            brief.answered = 1;
        }
        brief.confidence = if lifted {
            Some(relevance::LOW_CONFIDENCE_LINE.to_string())
        } else {
            got.scored
                .as_ref()
                .and_then(relevance::Verdict::confidence_line)
        };
        brief.absorb(fused, admit_json);
        let (wants_tests, wants_docs) = answer::cues(&plan.typed);
        brief.wants_tests = wants_tests || plan.kind == QuestionKind::Tests;
        brief.wants_docs = wants_docs;
        // Tests, docs and eval files rank after the code files unless the
        // question asked for them: a stable partition, nothing is dropped.
        let (wants_tests, wants_docs) = (brief.wants_tests, brief.wants_docs);
        brief
            .files
            .sort_by_key(|hit| answer::demoted(&hit.path, wants_tests, wants_docs));
        brief.zero_rule = answer::ZERO_RULE.filter(|_| answer::zero_enabled());
        brief.leads = leads.to_vec();
        brief.lexical = lines.iter().map(|hit| hit.path.clone()).collect();
        brief.receipt = match &got.relevance {
            Some(Ok(answer)) => {
                let chunks = match &got.meaning {
                    Some(Ok(leads)) => Some(leads.len()),
                    _ => None,
                };
                Receipt::new(&answer.input, chunks)
            }
            _ => None,
        };
    });
}

/// [`fold_with`] for a prompt of no session.
#[cfg(test)]
fn fold(state: &Mutex<Brief>, plan: &Plan, got: &Gathered) {
    fold_with(state, plan, got, &[], false);
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
///
/// `session` is the files the session is already working on, newest first. It
/// is a third list that only lifts: a file the leads and co-files already name
/// gains its reciprocal rank, and a file only the session names stays out, so
/// an unrelated dirty file never displaces a lead. With `only_session` (a short
/// follow-up the words cannot place) the session's files are the answer.
fn fuse_with(
    leads: &[MeaningHit],
    cofiles: &[RichHit],
    session: &[String],
    only_session: bool,
) -> Vec<RichHit> {
    fn add(ranked: &mut Vec<(RichHit, f64)>, hit: RichHit, rank: usize) {
        let score = reciprocal_rank(rank);
        match ranked.iter_mut().find(|(held, _)| held.path == hit.path) {
            Some((_, total)) => *total += score,
            None => ranked.push((hit, score)),
        }
    }
    let mut ranked: Vec<(RichHit, f64)> = Vec::new();
    if only_session {
        for (rank, path) in session.iter().enumerate() {
            let hit = RichHit {
                path: path.clone(),
                line: 0,
                text: None,
            };
            add(&mut ranked, hit, rank);
        }
    } else {
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
        for (rank, path) in session.iter().enumerate() {
            if let Some((_, total)) = ranked.iter_mut().find(|(held, _)| held.path == *path) {
                *total += reciprocal_rank(rank);
            }
        }
    }
    ranked.retain(|(hit, _)| !pixel_index::index::credential_path(Path::new(&hit.path)));
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    ranked
        .into_iter()
        .map(|(hit, _)| hit)
        .take(MAX_FILES)
        .collect()
}

/// [`fuse_with`] for a prompt of no session.
#[cfg(test)]
fn fuse(leads: &[MeaningHit], cofiles: &[RichHit]) -> Vec<RichHit> {
    fuse_with(leads, cofiles, &[], false)
}

/// Words a short follow-up leans on what came before.
const FOLLOW_UP_CUES: &[&str] = &[
    "now",
    "then",
    "also",
    "again",
    "same",
    "that",
    "this",
    "it",
    "them",
    "those",
    "there",
    "too",
    "next",
    "instead",
    "still",
    "maintenant",
    "ensuite",
    "aussi",
    "encore",
    "pareil",
    "ça",
    "cela",
];
/// The most words a follow-up has.
const FOLLOW_UP_WORDS: usize = 10;

/// Whether `typed` is a short follow-up: a few words that ask for a change or
/// lean on what came before ("now fix the test"). Such a prompt names nothing
/// the repository's words can place, which is why the gate closes it; with the
/// files the session is working on it can be placed.
pub(crate) fn is_follow_up(typed: &str) -> bool {
    let words: Vec<String> = typed
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    !words.is_empty()
        && words.len() <= FOLLOW_UP_WORDS
        && (has_change_intent(typed)
            || words
                .iter()
                .any(|word| FOLLOW_UP_CUES.contains(&word.as_str())))
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
    /// Lines of each excerpt block (at most [`EXCERPT_FILES`] blocks).
    excerpt: [usize; EXCERPT_FILES],
}

impl Shown {
    fn of(brief: &Brief) -> Self {
        Self {
            files: brief.files.len().min(if brief.has_excerpts() {
                ANSWER_FILES
            } else {
                MAX_FILES
            }),
            defined: brief.defined.len().min(MAX_DEFINED),
            callers: brief.callers.len().min(MAX_CALLERS),
            tests: brief.tests.len().min(MAX_FILES),
            skeleton: brief.skeleton.len().min(MAX_FILES),
            history: brief.history.len().min(HISTORY_ROWS),
            targets: brief.targets.len().min(MAX_TARGETS),
            excluded: brief.excluded.len(),
            caps: brief.caps.len(),
            unresolved: brief.unresolved.len(),
            excerpt: brief.excerpt_counts(),
        }
    }

    /// Drop one excerpt line, else one entry from the longest list (the earlier of equals in the
    /// order excluded, files, tests, targets, skeleton, history, callers,
    /// defined, caps, unresolved); `false` when every list is already empty.
    fn shrink(&mut self) -> bool {
        // The excerpts give way first, the second file's before the first's;
        // the receipt never does.
        if let Some(slot) = self.excerpt.iter_mut().rev().find(|slot| **slot > 0) {
            *slot -= 1;
            return true;
        }
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
#[cfg(test)]
pub(crate) fn render(brief: &Brief) -> Option<String> {
    fit(brief).map(|(text, _)| text)
}

/// What a rendered brief carried of the answer evidence, for the decision log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AnswerStats {
    /// The search receipt is in the block.
    pub(crate) receipt: bool,
    /// At least one excerpt line is in the block.
    pub(crate) answer: bool,
    /// Bytes of the excerpt lines (and their headers) in the block.
    pub(crate) excerpt_bytes: usize,
    /// Bytes of the region pack, when the block is one.
    pub(crate) pack_bytes: usize,
    /// Hook parts the pack's text fills.
    pub(crate) pack_parts: usize,
    /// Symbols the pack is about.
    pub(crate) pack_targets: usize,
    /// Blocks a locator contributed to the pack.
    pub(crate) pack_located: usize,
    /// The text search stood in for a graph that could not answer.
    pub(crate) fallback: bool,
}

/// [`render`], with the answer evidence it kept.
pub(crate) fn fit(brief: &Brief) -> Option<(String, AnswerStats)> {
    if brief.silenced || brief.answered == 0 {
        return None;
    }
    let low = brief.tier() == Some(Tier::Low);
    if low && !brief.pack_limit.on() {
        return render_low(brief).map(|text| (text, AnswerStats::default()));
    }
    if let Some(pack) = brief
        .pack
        .as_ref()
        .filter(|pack| brief.pack_limit.on() && pack.has_evidence())
    {
        // The packet was cut short: the pack says it is a lead, not an answer.
        let text = pack.text_hedged(brief.cut || low);
        let stats = AnswerStats {
            answer: true,
            excerpt_bytes: text.len(),
            pack_bytes: text.len(),
            pack_parts: crate::brief_parts::split(
                &text,
                crate::brief_parts::Limits {
                    chars: brief.pack_limit.chars,
                    bytes: usize::MAX,
                },
            )
            .len(),
            pack_targets: pack.targets(),
            pack_located: pack.located(),
            fallback: pack.fallback(),
            ..AnswerStats::default()
        };
        return Some((text, stats));
    }
    if low {
        return render_low(brief).map(|text| (text, AnswerStats::default()));
    }
    // Admitted for its kind, not for the evidence of the packet: without the
    // kind's own pack there is nothing to say, unless the window ended first.
    // Then the question was judged about this repository and the pack was
    // simply not ready: the best files it had found are a lead, which is
    // better than the agent hearing nothing.
    if matches!(brief.admission, Admission::Kind(_)) {
        return if brief.cut {
            // Whatever the relevance probe had to say, this is a lead and says so.
            let mut lead = brief.clone();
            lead.confidence
                .get_or_insert_with(|| relevance::LOW_CONFIDENCE_LINE.to_string());
            render_low(&lead).map(|text| (text, AnswerStats::default()))
        } else {
            None
        };
    }
    let receipt = !brief.receipt_lines().is_empty();
    let cap = byte_cap(
        brief.signal,
        receipt || brief.has_excerpts() || !brief.read_ranges().is_empty(),
    );
    // A shown answer may use the full block.
    let cap = if brief.zero() {
        cap.max(PROSE_BRIEF_BYTES)
    } else {
        cap
    };
    let mut shown = Shown::of(brief);
    let mut text = render_with(brief, shown);
    while text.len() > cap && shown.shrink() {
        text = render_with(brief, shown);
    }
    let excerpt = brief.excerpt_block(shown);
    let stats = AnswerStats {
        receipt,
        answer: !excerpt.is_empty(),
        excerpt_bytes: excerpt.iter().map(|line| line.len() + 1).sum(),
        ..AnswerStats::default()
    };
    Some((text, stats))
}

/// One entry of a `files:` line: `path:line`, and the matched text after a
/// dash when the source carried it (cut to `text_chars`). A hit with no line
/// of its own is a bare path.
fn file_entry(hit: &RichHit, text_chars: usize) -> String {
    let site = if hit.line == 0 {
        clean(&hit.path)
    } else {
        format!("{}:{}", clean(&hit.path), hit.line)
    };
    match &hit.text {
        Some(text) if text_chars > 0 => format!("{site} — {}", clean_n(text, text_chars)),
        _ => site,
    }
}

/// The compact block of a low-tier prompt: at most [`LOW_FILES`] files and
/// the line that says it is a maybe, within [`LOW_BRIEF_BYTES`]. Texts give
/// way first, from the full width to half and then to none, and files only
/// when paths alone are too long; `None` when there is no file to name.
fn render_low(brief: &Brief) -> Option<String> {
    let shown = brief.files.len().min(LOW_FILES);
    if shown == 0 {
        return None;
    }
    let confidence = brief.confidence.as_deref().unwrap_or_default();
    let reads = brief.read_lines();
    let block = |files: usize, text_chars: usize| {
        let entries: Vec<String> = brief
            .files
            .iter()
            .take(files)
            .map(|hit| file_entry(hit, text_chars))
            .collect();
        let mut text = format!("{BRIEF_TAG}\nfiles: {}\n{confidence}", entries.join("; "));
        for line in &reads {
            text.push('\n');
            text.push_str(line);
        }
        text
    };
    let cap = if reads.is_empty() {
        LOW_BRIEF_BYTES
    } else {
        LOW_READ_BRIEF_BYTES
    };
    let mut text = block(1, 0);
    for files in (1..=shown).rev() {
        for text_chars in [MAX_ITEM_CHARS, MAX_ITEM_CHARS / 2, 0] {
            text = block(files, text_chars);
            if text.len() <= cap {
                return Some(text);
            }
        }
    }
    Some(text)
}

/// The size cap of a brief started by `signal`; `answer`: it carries a
/// receipt or excerpts, which raises the cap of a plain-language prompt and
/// of a weak one.
fn byte_cap(signal: Option<Signal>, answer: bool) -> usize {
    match (signal, answer) {
        (Some(Signal::Prose | Signal::Weak), true) => PROSE_BRIEF_BYTES,
        (Some(Signal::Prose), false) => PLAIN_PROSE_BRIEF_BYTES,
        (Some(Signal::Strong), true) => STRONG_READ_BRIEF_BYTES,
        _ => BRIEF_BYTES,
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
    lines.extend(brief.receipt_lines());
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
        // Evidence that did not earn the ZERO step is a medium confidence.
        lines.push(if brief.confident() {
            confidence.clone()
        } else if brief.read_ranges().is_empty() {
            MEDIUM_FILES_CONFIDENCE.to_string()
        } else {
            MEDIUM_CONFIDENCE.to_string()
        });
    }
    lines.extend(brief.excerpt_block(shown));
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
    if brief.partial() {
        lines.push("packet partial — open cited regions or run the named op".to_string());
        if let Some(next) = &brief.next {
            lines.push(format!("next: {}", clean_n(next, DEF_BODY_CHARS)));
        }
    }
    // The brief ends on one concrete instruction: answer from the excerpt
    // (ZERO), or read the named ranges (ONE READ).
    let reads = brief.read_lines();
    if brief.zero() {
        lines.push(ZERO_LINE.to_string());
    } else if !reads.is_empty() {
        lines.extend(reads);
    } else if !brief.partial() {
        lines.push(
            if brief.confident() {
                FOOTER
            } else {
                UNCORROBORATED_FOOTER
            }
            .to_string(),
        );
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
            ..HistoryHit::default()
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
        // Codex's envelopes carry the identifiers of whatever they relay.
        let delegation = [
            "<realtime_delegation>",
            "  <input>where is fetchUser</input>",
            "</realtime_delegation>",
        ]
        .join("\n");
        let reply = [
            "<send_user_message_question_reply>",
            "[{\"question\":\"where is fetchUser\",\"answer\":\"src/api.rs\"}]",
            "</send_user_message_question_reply>",
        ]
        .join("\n");
        let reminded = format!("<system-reminder>ctx</system-reminder>\n{delegation}");
        for envelope in [
            "<task-notification><task-id>x</task-id>fetchUser retry_count failed</task-notification>",
            "<system-reminder>callers of `fetchUser`</system-reminder>",
            "<command-name>/review</command-name> `fetchUser`",
            delegation.as_str(),
            reply.as_str(),
            reminded.as_str(),
        ] {
            assert_eq!(Plan::from_prompt(envelope), None, "{envelope}");
        }
        assert!(
            Plan::from_prompt("fix the hook: a <task-notification> reaches `fetchUser`").is_some()
        );
        assert!(
            Plan::from_prompt("fix the hook: a <realtime_delegation> reaches `fetchUser`")
                .is_some()
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
        /// The plain-word leads.
        words: Result<Vec<MeaningHit>, String>,
        /// The semantic hint the fake advertises, when it models a warm index.
        semantic: Option<String>,
        /// Source line a `line_at` read answers with, when the fake models
        /// source at all.
        source: Option<String>,
        /// Files a `lines_at` read answers from, by path.
        sources: Vec<(String, Vec<String>)>,
        pause: Duration,
        /// The plugins the fake models as running.
        plugins: pixel_plugin::Available,
        log: Arc<Mutex<Vec<String>>>,
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
                words: Err("unavailable".into()),
                semantic: None,
                source: None,
                sources: Vec::new(),
                pause: Duration::ZERO,
                plugins: pixel_plugin::Available::none(),
                log: Arc::new(Mutex::new(Vec::new())),
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
        fn plugins(&self) -> pixel_plugin::Available {
            self.plugins.clone()
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
        fn word_leads(&self, _: &str, _: usize, _: Instant) -> Result<Vec<MeaningHit>, String> {
            self.words.clone()
        }
        fn semantic_hint(&self, phrase: &str) -> Option<String> {
            self.note(format!("semantic_hint {phrase}"));
            self.semantic.clone()
        }
        fn line_at(&self, path: &str, line: u64, _: Instant) -> Result<String, String> {
            self.note(format!("line_at {path}:{line}"));
            self.source.clone().ok_or_else(|| "no source".to_string())
        }
        fn lines_at(
            &self,
            path: &str,
            start: u64,
            end: u64,
            _: Instant,
        ) -> Result<Vec<String>, String> {
            self.note(format!("lines_at {path}:{start}-{end}"));
            let (_, all) = self
                .sources
                .iter()
                .find(|(name, _)| name == path)
                .ok_or_else(|| "no source".to_string())?;
            Ok(all
                .iter()
                .skip(usize::try_from(start.saturating_sub(1)).unwrap_or(0))
                .take(usize::try_from(end + 1 - start).unwrap_or(0))
                .cloned()
                .collect())
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
        fn word_leads(
            &self,
            query: &str,
            limit: usize,
            deadline: Instant,
        ) -> Result<Vec<MeaningHit>, String> {
            self.0.word_leads(query, limit, deadline)
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
    fn chain_should_list_files_named_by_path_first_and_never_look_up_their_stem() {
        let mut fake = Fake::new();
        fake.source = Some("/** head */".into());
        fake.files = Ok(found(vec![hit(".agents/skills/arena/SKILL.md", 10)]));
        let brief = chain(
            "summarize extensions/plan/index.ts and extensions/todo/index.ts",
            &fake,
            SECOND,
        );
        let calls = fake.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("symbols") || c.contains("files_with")),
            "{calls:?}"
        );
        assert_eq!(
            brief.files,
            [
                RichHit {
                    path: "extensions/plan/index.ts".into(),
                    line: 1,
                    text: Some("/** head */".into())
                },
                RichHit {
                    path: "extensions/todo/index.ts".into(),
                    line: 1,
                    text: Some("/** head */".into())
                },
            ]
        );
        assert!(brief.unresolved.is_empty() && brief.likely_def.is_none());
        assert!(brief.answered > 0 && brief.finished && !brief.partial());
    }

    #[test]
    fn chain_should_not_pin_a_path_when_the_prompt_names_an_identifier() {
        let mut fake = Fake::new();
        fake.source = Some("import x".into());
        fake.files = Ok(found(vec![hit("README.md", 3)]));
        let brief = chain(
            "explain src/subagent/scheduler.ts and scheduleLoop()",
            &fake,
            SECOND,
        );
        assert!(
            brief
                .files
                .iter()
                .all(|f| f.path != "src/subagent/scheduler.ts")
        );
    }

    #[test]
    fn anchors_should_not_take_a_generic_stem_as_a_symbol() {
        for text in [
            "explain src/a/index.ts",
            "explain src/a/mod.rs",
            "read a/b/index.ts and c/d/index.ts",
        ] {
            let found = Anchors::from_text(text);
            assert_eq!(found.symbol_name(), None, "{text}");
            assert_eq!(found.search_term(), None, "{text}");
        }
        let one = Anchors::from_text("explain src/subagent/scheduler.ts");
        assert_eq!(one.symbol_name().as_deref(), Some("scheduler"));
        let two = Anchors::from_text("explain src/a/scheduler.ts and src/b/runner.ts");
        assert_eq!(two.symbol_name(), None);
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
    fn a_rationale_prompt_should_read_history_but_name_no_command_when_its_plugin_does_not_run() {
        let mut fake = Fake::new();
        fake.status = Ok(StatusProbe {
            facts_fresh: Some(true),
        });
        fake.history = Ok((
            vec![history("abc12345", "introduce the retry loop")],
            Vec::new(),
        ));
        let brief = chain("when was `retry_loop` introduced", &fake, SECOND);
        assert_eq!(brief.history.len(), 1, "the library read needs no plugin");
        // The concept search's own next step stands; the history command is not added.
        assert_eq!(
            brief.next.as_deref(),
            Some("pixel find-code 'retry loop introduced'")
        );
        let text = render(&brief).unwrap();
        assert!(!text.contains("dig-history"), "{text}");
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
        fake.plugins = pixel_plugin::Available::of(&["dig-history"]);
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
            excerpt: [0, 0, 0],
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
            excerpt: [0, 0, 0],
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

    /// The repository talks about the prompt: one structural file holds all
    /// its words (2 each), a note holds two of them as text.
    fn on_topic_answer() -> RelevanceAnswer {
        RelevanceAnswer {
            input: RelevanceInput {
                graph: true,
                files_considered: 1000,
                structural_files: 30,
                keywords: vec![
                    keyword("daemon", 2.0),
                    keyword("changes", 2.0),
                    keyword("startup", 2.0),
                ],
                cofiles: vec![
                    cofile(DAEMON, &["daemon", "changes", "startup"], 6.0, true),
                    cofile("docs/notes.md", &["daemon", "startup"], 4.0, false),
                ],
            },
            lines: vec![
                rhit(DAEMON, 280, "fn watch_ready"),
                rhit("docs/notes.md", 12, "daemon startup notes"),
            ],
        }
    }

    /// The `confidence:` line of a high-tier brief the stub models decide.
    const HIGH_CONFIDENCE: &str =
        "confidence: high — 3/3 key terms covered; start with the first file";
    const LOW_CONFIDENCE: &str = "confidence: low — possibly related; verify before relying on it";

    /// The repository does not talk about the prompt: two of its words are
    /// nowhere (the cap), the third is everywhere (0), and no file holds any.
    fn off_topic_answer() -> RelevanceAnswer {
        RelevanceAnswer {
            input: RelevanceInput {
                graph: true,
                files_considered: 1000,
                structural_files: 0,
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

    /// The verdict of a model that puts every prompt in `tier`: the
    /// pipeline's tests do not depend on the constants of the real one.
    fn verdict_in(tier: Tier, input: &GateInput) -> relevance::Verdict {
        relevance::Verdict {
            tier,
            basis: Basis::Model,
            score: Some(match tier {
                Tier::High => 2.0,
                Tier::Low => 1.0,
                Tier::Off => -1.0,
            }),
            best_file: input
                .relevance
                .cofiles
                .first()
                .map(|file| file.path.clone()),
            features: relevance::Features {
                keywords: 3,
                informative: 3,
                shared: 3,
                ..relevance::Features::default()
            },
        }
    }

    fn model_high(input: &GateInput) -> relevance::Verdict {
        verdict_in(Tier::High, input)
    }

    fn model_low(input: &GateInput) -> relevance::Verdict {
        verdict_in(Tier::Low, input)
    }

    fn model_off(input: &GateInput) -> relevance::Verdict {
        verdict_in(Tier::Off, input)
    }

    /// A model that finds nothing to say: the code graph did not answer.
    fn model_no_graph(input: &GateInput) -> relevance::Verdict {
        relevance::Verdict {
            basis: Basis::NoGraph,
            score: None,
            ..verdict_in(Tier::Off, input)
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

    /// Start `prompt` on `fake` with `judge` and `model`, and wait for the brief.
    fn briefed_with(
        prompt: &str,
        fake: Fake,
        judge: impl Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
        model: Model,
    ) -> Finished {
        try_start_with(
            prompt,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            judge,
            model,
            Around::default(),
        )
        .expect("the prompt starts a brief")
        .finish_with_record()
    }

    /// [`briefed_with`] a model that finds the prompt on topic.
    fn briefed(
        prompt: &str,
        fake: Fake,
        judge: impl Fn(&str, Instant) -> Option<Verdict> + Send + Sync + 'static,
    ) -> Finished {
        briefed_with(prompt, fake, judge, model_high)
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
        let finished = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(Shared(fake)),
            no_verdict,
            model_high,
            Around::default(),
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
                    "searched: content+symbols+paths for daemon, changes, startup (3 terms, 1000 files) · meaning search returned 2 chunks",
                    "result: the matches below are the best across both searches; verify they answer the question",
                    "files: crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready() {; crates/pixel-daemon/src/api.rs:40 — fn op_status; docs/notes.md:12 — daemon startup notes",
                    MEDIUM_FILES_CONFIDENCE,
                    "coverage: 2/2 ops answered",
                    UNCORROBORATED_FOOTER,
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
        assert_eq!((record.tier, record.score), (Some("high"), Some(2.0)));
        assert_eq!((record.ops, record.answered), (2, 2));
        let features = record.features.as_ref().unwrap();
        assert_eq!((features.shared, features.informative), (3, 3));
        assert_eq!(record.bytes, finished.text.unwrap().len());
        assert_eq!(record.typed, PROSE);
        assert_eq!(record.sha256, decision_log::sha256_hex(PROSE));
    }

    #[test]
    fn a_prose_prompt_without_meaning_leads_should_fuse_the_plain_word_leads_in_their_place() {
        // No model on disk: the meaning search fails, and the file only the
        // plain words found joins the co-files in the fusion, ahead of the
        // doc the co-files name second. Without the fallback it is absent.
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.words = Ok(vec![lead("crates/pixel-daemon/src/api.rs", 40, "")]);
        let finished = briefed(PROSE, fake, no_verdict);
        assert_eq!(
            files_line(finished.text.as_deref().unwrap_or_default()),
            "files: crates/pixel-daemon/src/api.rs:40; crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready; docs/notes.md:12 — daemon startup notes"
        );
        // The receipt still says the meaning search did not answer: the
        // plain words are not counted as one.
        let text = finished.text.unwrap();
        assert!(!text.contains("meaning search returned"), "{text}");
        assert!(text.contains("coverage: 1/2 ops answered"), "{text}");
    }

    #[test]
    fn a_prose_prompt_with_meaning_leads_should_never_read_the_plain_word_leads() {
        // The fallback is a fallback: when the meaning search answers, the
        // brief is byte for byte what it was before the plain words existed.
        let with_words = || {
            let mut fake = Fake::new();
            fake.relevance = Ok(on_topic_answer());
            fake.meaning = Ok(vec![lead(DAEMON, 280, "fn watch_ready() {")]);
            fake.words = Ok(vec![lead("crates/pixel-daemon/src/api.rs", 40, "")]);
            fake
        };
        let mut without = with_words();
        without.words = Err("unavailable".into());
        let with = briefed(PROSE, with_words(), no_verdict).text;
        assert!(
            !with.as_deref().unwrap_or_default().contains("api.rs"),
            "{with:?}"
        );
        assert_eq!(with, briefed(PROSE, without, no_verdict).text);
    }

    #[test]
    fn a_prose_prompt_the_model_puts_off_should_render_nothing_and_say_why() {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        let finished = briefed_with(
            "what's the weather going to be like tomorrow",
            fake,
            no_verdict,
            model_off,
        );
        assert_eq!(finished.text, None);
        let record = finished.record;
        assert_eq!((record.signal, record.gate), (Some("prose"), "closed"));
        assert_eq!((record.tier, record.score), (Some("off"), Some(-1.0)));
        assert_eq!(
            record.reason.as_deref(),
            Some("off topic: score -1.00, 3/3 key terms in the best file")
        );
        assert_eq!((record.answered, record.bytes), (0, 0));
    }

    #[test]
    fn a_prose_prompt_whose_relevance_probe_fails_should_not_wait_for_the_slow_meaning_search() {
        let mut fake = Fake::new();
        fake.relevance = Err("cold".into());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        fake.meaning_pause = Duration::from_millis(1200);
        let started = Instant::now();
        let finished = try_start_with(
            PROSE,
            OPEN,
            Duration::from_secs(5),
            move |_| Box::new(fake),
            no_verdict,
            model_high,
            Around::default(),
        )
        .unwrap()
        .finish_with_record();
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.gate, "closed");
        assert_eq!(finished.record.reason.as_deref(), Some("relevance: cold"));
    }

    #[test]
    fn the_intent_judge_should_have_no_say_in_whether_a_prose_prompt_is_briefed() {
        for label in ["none", "ops", "question"] {
            let mut fake = Fake::new();
            fake.relevance = Ok(on_topic_answer());
            fake.meaning = Ok(vec![lead(DAEMON, 280, "fn watch_ready")]);
            let finished = briefed(PROSE, fake, verdict(label));
            let text = finished
                .text
                .unwrap_or_else(|| panic!("{label} silenced it"));
            assert!(
                text.contains(&format!("\nintent: {label} (0.90)\n")),
                "{text}"
            );
            assert_eq!(finished.record.gate, "open", "{label}");
            assert_eq!(
                finished.record.judge,
                Some((label.to_string(), 0.9)),
                "{label}"
            );
        }
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
    fn a_weak_prompt_the_model_puts_off_should_get_no_brief() {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let finished = briefed_with("how does the login flow work", fake, no_verdict, model_off);
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.gate, "closed");
        assert_eq!(finished.record.tier, Some("off"));
        assert!(finished.record.enforced);
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
            model_high,
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
            model_high,
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
        fake.relevance = Err("cold".into());
        let got = gather(
            PROSE,
            &fake,
            &no_verdict,
            model_high,
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

    /// What the probes answered, scored by a model that puts the prompt in `tier`.
    fn gathered(
        relevance: Option<Result<RelevanceAnswer, String>>,
        judged: Option<Option<Verdict>>,
        tier: Tier,
    ) -> Gathered {
        let scored = relevance
            .as_ref()
            .and_then(|answer| answer.as_ref().ok())
            .map(|answer| {
                verdict_in(
                    tier,
                    &GateInput {
                        relevance: &answer.input,
                        typed: PROSE,
                    },
                )
            });
        Gathered {
            relevance,
            scored,
            meaning: None,
            words: None,
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
        let got = gathered(Some(Ok(off_topic_answer())), said("none", 0.99), Tier::Off);
        assert_eq!(
            refusal(Signal::Strong, &got, true, true, QuestionKind::Lookup),
            None
        );
    }

    #[test]
    fn refusal_should_close_a_prose_prompt_the_model_puts_off() {
        let off = gathered(Some(Ok(off_topic_answer())), None, Tier::Off);
        assert_eq!(
            refusal(Signal::Prose, &off, false, false, QuestionKind::Lookup),
            Some(Admission::Closed(
                "off topic: score -1.00, 3/3 key terms in the best file".into()
            ))
        );
        let failed = gathered(Some(Err("cold".into())), None, Tier::Off);
        assert_eq!(
            refusal(Signal::Prose, &failed, false, false, QuestionKind::Lookup),
            Some(Admission::Closed("relevance: cold".into()))
        );
        for tier in [Tier::High, Tier::Low] {
            let on = gathered(Some(Ok(on_topic_answer())), None, tier);
            assert_eq!(
                refusal(Signal::Prose, &on, true, false, QuestionKind::Lookup),
                None,
                "{tier:?}"
            );
        }
    }

    #[test]
    fn refusal_should_wait_for_a_missing_relevance_answer_until_nothing_more_is_coming() {
        let silent = gathered(None, Some(None), Tier::Off);
        assert_eq!(
            refusal(Signal::Prose, &silent, false, false, QuestionKind::Lookup),
            None
        );
        assert_eq!(
            refusal(Signal::Prose, &silent, true, false, QuestionKind::Lookup),
            Some(Admission::Closed(
                "relevance: no answer before the deadline".into()
            ))
        );
    }

    #[test]
    fn refusal_should_leave_a_prose_prompt_to_the_model_whatever_the_intent_judge_says() {
        for label in ["none", "ops"] {
            let on = gathered(Some(Ok(on_topic_answer())), said(label, 0.99), Tier::High);
            assert_eq!(
                refusal(Signal::Prose, &on, true, false, QuestionKind::Lookup),
                None,
                "{label}"
            );
        }
        let unanswered = gathered(None, said("none", 0.99), Tier::High);
        assert_eq!(
            refusal(
                Signal::Prose,
                &unanswered,
                false,
                false,
                QuestionKind::Lookup
            ),
            None,
            "a judge that answers first does not close the brief"
        );
    }

    #[test]
    fn refusal_lifting_should_not_close_a_lifted_prose_prompt_whatever_the_probe_says() {
        let lookup = QuestionKind::Lookup;
        let failed = gathered(Some(Err("facts: index_stale".into())), None, Tier::Off);
        let silent = gathered(None, Some(None), Tier::Off);
        let off = gathered(Some(Ok(off_topic_answer())), None, Tier::Off);
        for (got, complete, closed) in [
            (&failed, false, "relevance: facts: index_stale"),
            (&failed, true, "relevance: facts: index_stale"),
            (&silent, true, "relevance: no answer before the deadline"),
            (
                &off,
                false,
                "off topic: score -1.00, 3/3 key terms in the best file",
            ),
        ] {
            assert_eq!(
                refusal_lifting(Signal::Prose, got, complete, false, lookup, false),
                Some(Admission::Closed(closed.into())),
                "unlifted: {closed}"
            );
            assert_eq!(
                refusal_lifting(Signal::Prose, got, complete, false, lookup, true),
                None,
                "lifted: {closed}"
            );
        }
    }

    #[test]
    fn refusal_lifting_should_leave_a_judge_denial_standing_for_a_lifted_weak_prompt() {
        let denied = gathered(Some(Ok(on_topic_answer())), said("none", 0.9), Tier::High);
        assert_eq!(
            refusal_lifting(
                Signal::Weak,
                &denied,
                true,
                false,
                QuestionKind::Lookup,
                true
            ),
            Some(Admission::Denied("judge: none (0.90)".into()))
        );
    }

    #[test]
    fn refusal_should_deny_a_weak_prompt_only_on_none_and_close_it_only_when_enforced() {
        let on = |tier| Some(Ok(on_topic_answer())).map(|answer| (answer, tier));
        let on_gathered = |judged, tier| {
            let (answer, tier) = on(tier).unwrap();
            gathered(Some(answer), judged, tier)
        };
        let off = || gathered(Some(Ok(off_topic_answer())), None, Tier::Off);
        assert_eq!(
            refusal(
                Signal::Weak,
                &on_gathered(said("none", 0.9), Tier::High),
                true,
                false,
                QuestionKind::Lookup
            ),
            Some(Admission::Denied("judge: none (0.90)".into()))
        );
        assert_eq!(
            refusal(
                Signal::Weak,
                &on_gathered(said("ops", 0.99), Tier::High),
                true,
                false,
                QuestionKind::Lookup
            ),
            None,
            "ops steers a weak prompt, it never silences it"
        );
        assert_eq!(
            refusal(Signal::Weak, &off(), true, false, QuestionKind::Lookup),
            None
        );
        assert_eq!(
            refusal(Signal::Weak, &off(), true, true, QuestionKind::Lookup),
            Some(Admission::Closed(
                "off topic: score -1.00, 3/3 key terms in the best file".into()
            ))
        );
        for tier in [Tier::High, Tier::Low] {
            assert_eq!(
                refusal(
                    Signal::Weak,
                    &on_gathered(None, tier),
                    true,
                    true,
                    QuestionKind::Lookup
                ),
                None,
                "{tier:?}"
            );
        }
        let failed = gathered(Some(Err("cold".into())), None, Tier::Off);
        assert_eq!(
            refusal(Signal::Weak, &failed, true, true, QuestionKind::Lookup),
            None,
            "a probe that cannot tell never silences a weak prompt"
        );
        assert_eq!(
            refusal(
                Signal::Weak,
                &gathered(None, None, Tier::Off),
                true,
                true,
                QuestionKind::Lookup
            ),
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
        assert_eq!(
            byte_cap(Some(Signal::Prose), false),
            PLAIN_PROSE_BRIEF_BYTES
        );
        assert_eq!(byte_cap(Some(Signal::Prose), true), PROSE_BRIEF_BYTES);
        assert_eq!(byte_cap(Some(Signal::Weak), true), PROSE_BRIEF_BYTES);
        assert_eq!(
            byte_cap(Some(Signal::Strong), true),
            STRONG_READ_BRIEF_BYTES
        );
        assert_eq!(byte_cap(Some(Signal::Strong), false), BRIEF_BYTES);
        assert_eq!(byte_cap(Some(Signal::Weak), false), BRIEF_BYTES);
        assert_eq!(byte_cap(None, false), BRIEF_BYTES);
        assert_eq!(PLAIN_PROSE_BRIEF_BYTES, 2048);
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
        assert!(text.len() <= PLAIN_PROSE_BRIEF_BYTES, "{}", text.len());
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
    fn kick_index_should_launch_once_for_an_unindexed_code_prompt() {
        let root = Path::new("/repo");
        let mut launched = Vec::new();
        kick_index(
            Some(&Declined::Unindexed),
            "where is retrieval_route called",
            root,
            true,
            |launched_root| launched.push(launched_root.to_path_buf()),
        );
        assert_eq!(launched, vec![root.to_path_buf()]);
    }

    #[test]
    fn kick_index_should_not_launch_when_daemon_auto_start_is_off() {
        let mut launches = 0;
        kick_index(
            Some(&Declined::Unindexed),
            "where is retrieval_route called",
            Path::new("/repo"),
            false,
            |_| launches += 1,
        );
        assert_eq!(launches, 0);
    }

    #[test]
    fn kick_index_should_not_launch_for_a_continuation_a_plain_prompt_or_another_decline() {
        let root = Path::new("/repo");
        for (declined, prompt) in [
            (Some(&Declined::Unindexed), "thanks"),
            (Some(&Declined::Unindexed), "hello there"),
            (
                Some(&Declined::NotAboutCode),
                "where is retrieval_route called",
            ),
            (Some(&Declined::Disabled), "where is retrieval_route called"),
            (None, "where is retrieval_route called"),
        ] {
            let mut launches = 0;
            kick_index(declined, prompt, root, true, |_| launches += 1);
            assert_eq!(launches, 0, "{declined:?} {prompt:?}");
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
            try_start_with(
                prompt,
                gate,
                SECOND,
                open,
                no_verdict,
                model_high,
                Around::default(),
            )
            .err()
            .map(Declined::as_str)
        };
        assert_eq!(why("callers of `fetchUser`", closed), Some("disabled"));
        assert_eq!(why("callers of `fetchUser`", unindexed), Some("unindexed"));
        assert_eq!(why("ok", OPEN), Some("continuation"));
        // Codex's envelopes are the harness speaking, whatever their body names.
        let delegation = [
            "<realtime_delegation>",
            "  <input>where is fetchUser</input>",
            "</realtime_delegation>",
        ]
        .join("\n");
        let reply = [
            "<send_user_message_question_reply>",
            "[{\"question\":\"where is fetchUser\",\"answer\":\"src/api.rs\"}]",
            "</send_user_message_question_reply>",
        ]
        .join("\n");
        assert_eq!(why(&delegation, OPEN), Some("continuation"));
        assert_eq!(why(&reply, OPEN), Some("continuation"));
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
            model_high,
            Around::logging(path.clone()),
        )
        .unwrap()
        .finish_with_record();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let mut logged = serde_json::from_str::<serde_json::Value>(lines[0]).unwrap();
        let provenance = logged
            .as_object_mut()
            .unwrap()
            .remove("provenance")
            .unwrap();
        assert_eq!(provenance["schema_version"], 1);
        assert_eq!(provenance["source"], "brief");
        assert_eq!(logged, finished.record.to_json());
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
            model_high,
            Around::default(),
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
                try_start_with(
                    prompt,
                    OPEN,
                    SECOND,
                    open,
                    no_verdict,
                    model_high,
                    Around::logging(path.clone()),
                )
                .is_err()
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
                model_high,
                Around::logging(path.clone()),
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
            model_high,
            Around::logging(path.clone()),
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

    /// A prompt that is only a pasted block: what the thread is about, a log
    /// nobody asks about, and what is wanted of it.
    fn pasted_only_prompt() -> String {
        [
            "<pasted_content id=\"1\">",
            PROSE,
            "",
            "a log that matters little",
            "",
            "and make the watcher wait for the first scan",
            "</pasted_content id=\"1\">",
        ]
        .join("\n")
    }

    #[test]
    fn a_prompt_that_is_only_a_pasted_block_should_be_briefed_on_its_head_and_tail() {
        let prompt = pasted_only_prompt();
        let task = [PROSE, "and make the watcher wait for the first scan"].join("\n");
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 280, "fn watch_ready() {")]);
        let fake = Arc::new(fake);
        let seen = Arc::clone(&fake);
        let finished = try_start_with(
            &prompt,
            OPEN,
            SECOND,
            move |_| Box::new(Shared(fake)),
            no_verdict,
            model_high,
            Around::default(),
        )
        .expect("a paste with a task starts a brief")
        .finish_with_record();
        // The probes read the task, never the log between its two ends.
        assert_eq!(
            sorted(seen.calls()),
            [
                format!("meaning {task} limit 8"),
                format!("relevance {task}")
            ]
        );
        let text = finished.text.expect("an on-topic paste is briefed");
        assert!(text.contains(DAEMON), "{text}");
        let record = finished.record;
        assert_eq!(
            (record.signal, record.gate, record.enforced),
            (Some("prose"), "open", true)
        );
        assert_eq!(record.typed, task);
        assert_eq!(record.sha256, decision_log::sha256_hex(&task));
    }

    #[test]
    fn a_pasted_block_should_still_meet_the_gate_and_reach_the_log_masked() {
        // Plain language, whatever the paste holds: the gate closes a paste
        // the repository is not about.
        let mut off = Fake::new();
        off.relevance = Ok(off_topic_answer());
        let closed = briefed_with(&pasted_only_prompt(), off, no_verdict, model_off);
        assert_eq!(closed.text, None);
        assert_eq!(
            (closed.record.signal, closed.record.gate),
            (Some("prose"), "closed")
        );
        // A credential in the paste is masked in the log like one typed.
        let leaking = [
            "<pasted_content>",
            "the daemon misses changes, the key is sk-abcdef123456 here",
            "</pasted_content>",
        ]
        .join("\n");
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let logged = briefed(&leaking, fake, no_verdict).record.typed;
        assert_eq!(
            logged,
            "the daemon misses changes, the key is <redacted> here"
        );
    }

    #[test]
    fn a_pasted_block_beside_typed_words_should_still_be_declined_as_not_about_code() {
        let beside =
            "<pasted_content>the daemon misses changes made during startup</pasted_content> thanks";
        let declined = try_start_with(
            beside,
            OPEN,
            SECOND,
            |_| Box::new(Fake::new()),
            no_verdict,
            model_high,
            Around::default(),
        );
        assert_eq!(declined.err(), Some(Declined::NotAboutCode));
        // Its record keeps the typed words only: the paste is not logged.
        let record = declined_record(Declined::NotAboutCode, beside);
        assert_eq!(record.typed, "thanks");
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
    fn a_brief_its_window_cut_should_log_cut_and_the_deadline_it_was_given() {
        let mut fake = Fake::new();
        fake.pause = Duration::from_millis(300);
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        let started = Instant::now();
        let record = try_start_with(
            "callers of `fetchUser`",
            OPEN,
            Duration::from_millis(60),
            move |_| Box::new(fake),
            no_verdict,
            model_high,
            Around::default(),
        )
        .expect("the prompt starts a brief")
        .finish_with_record()
        .record;
        assert!(started.elapsed() < Duration::from_millis(250));
        // Without these two, a brief that lost its evidence to the window
        // reads like a complete one in the log.
        assert!(record.cut);
        assert_eq!(record.deadline_ms, Some(60));
        // The worker was still in its first search when the window ended.
        assert_eq!(record.stages.search_ms, None);
        assert_eq!(record.stages.rank_ms, None);
        let value = record.to_json();
        assert_eq!(value["cut"], true);
        assert_eq!(value["deadline_ms"], 60);
    }

    #[test]
    fn a_complete_brief_should_log_each_stage_it_ran_and_no_cut() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.pause = Duration::from_millis(30);
        let record = briefed(PROSE, fake, no_verdict).record;
        assert!(!record.cut);
        assert_eq!(record.deadline_ms, Some(1000));
        let search = record.stages.search_ms.expect("the probes searched");
        assert!((30..1000).contains(&search), "{search}");
        assert!(record.stages.rank_ms.is_some_and(|ms| ms < 1000));
        assert!(record.stages.render_ms.is_some_and(|ms| ms < 1000));
        // No probe is configured here: the stage is absent, not zero.
        assert_eq!(record.stages.probe_ms, None);
        assert!(
            search <= record.elapsed_ms,
            "{search} > {}",
            record.elapsed_ms
        );
    }

    #[test]
    fn a_brief_the_gate_closes_should_log_its_search_but_no_rank() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let record = briefed("how does the login flow work", fake, verdict("none")).record;
        assert_eq!(record.gate, "denied");
        assert!(record.stages.search_ms.is_some());
        assert_eq!(record.stages.rank_ms, None);
    }

    #[test]
    fn a_declined_prompt_should_log_no_window_and_no_stage() {
        let record = declined_record(Declined::NotAboutCode, "thanks");
        assert!(!record.cut);
        assert_eq!(record.deadline_ms, None);
        assert_eq!(record.stages, Stages::default());
    }

    #[test]
    fn work_stages_should_split_the_worker_time_at_the_search_mark() {
        let started = Instant::now();
        let at = |ms| started + Duration::from_millis(ms);
        // Searched at 40, worker done at 100, collected at 120.
        assert_eq!(
            work_stages(started, Some(at(40)), Some(at(100)), at(120), true),
            (Some(40), Some(60))
        );
        // The window cut the worker: ranking ran until the brief was collected.
        assert_eq!(
            work_stages(started, Some(at(40)), None, at(120), true),
            (Some(40), Some(80))
        );
        // A worker mark after collection cannot stretch the stage.
        assert_eq!(
            work_stages(started, Some(at(40)), Some(at(200)), at(120), true),
            (Some(40), Some(80))
        );
        assert_eq!(
            work_stages(started, Some(at(40)), Some(at(100)), at(120), false),
            (Some(40), None)
        );
        assert_eq!(
            work_stages(started, None, Some(at(100)), at(120), true),
            (None, None)
        );
    }

    #[test]
    fn millis_should_count_whole_milliseconds_and_saturate() {
        assert_eq!(millis(Duration::from_micros(2_999)), 2);
        assert_eq!(millis(Duration::MAX), u64::MAX);
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
    fn a_weak_prompt_the_judge_refuses_before_the_relevance_probe_answers_should_say_so() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.pause = Duration::from_millis(300);
        let started = Instant::now();
        let finished = briefed("how does the login flow work", fake, verdict("none"));
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

    #[test]
    fn a_prose_prompt_in_the_low_tier_should_render_a_compact_maybe_and_run_no_route() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![
            lead(DAEMON, 280, "fn watch_ready() {"),
            lead("crates/pixel-daemon/src/api.rs", 40, "fn op_status"),
            lead("crates/pixel-daemon/src/x.rs", 1, "x"),
            lead("crates/pixel-daemon/src/y.rs", 2, "y"),
        ]);
        let log = Arc::clone(&fake.log);
        let finished = briefed_with(PROSE, fake, verdict("question"), model_low);
        // The notes file ranks after the code files, so the third slot is code.
        assert_eq!(
            finished.text.as_deref(),
            Some(
                [
                    "[PIXEL:BRIEF]",
                    "files: crates/pixel-daemon/src/daemon.rs:280 — fn watch_ready() {; crates/pixel-daemon/src/api.rs:40 — fn op_status; crates/pixel-daemon/src/x.rs:1 — x",
                    LOW_CONFIDENCE,
                ]
                .join("\n")
                .as_str()
            )
        );
        // The two probes only: no kind route, no search, no definition read
        // (the files are only read to fix the ranges to name).
        let probes: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|call| !call.starts_with("lines_at "))
            .cloned()
            .collect();
        assert_eq!(
            sorted(probes),
            [
                format!("meaning {PROSE} limit 8"),
                format!("relevance {PROSE}")
            ]
        );
        let record = finished.record;
        assert_eq!(
            (record.gate, record.tier, record.kind),
            ("open", Some("low"), None)
        );
        assert_eq!((record.ops, record.answered), (2, 2));
        assert!(record.bytes <= LOW_BRIEF_BYTES, "{}", record.bytes);
    }

    #[test]
    fn a_prose_prompt_in_the_low_tier_should_get_one_part_of_regions_as_a_lead() {
        let source: Vec<String> = [
            "/// Wait until the watcher has caught up with the startup scan.",
            "fn watch_ready() {",
            "    while !caught_up() {",
            "        std::thread::yield_now();",
            "    }",
            "    mark_ready();",
            "}",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 2, "fn watch_ready() {")]);
        fake.sources = vec![(DAEMON.to_string(), source)];
        let finished = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            verdict("question"),
            model_low,
            Around {
                pack: region_pack::Limit::of(3),
                ..Around::default()
            },
        )
        .expect("the prompt starts a brief")
        .finish_with_record();
        let text = finished.text.expect("a brief");
        assert!(text.starts_with("[PIXEL:BRIEF]\nregion pack"), "{text}");
        assert!(text.contains("2| fn watch_ready() {"), "{text}");
        assert!(
            text.ends_with(region_pack::LEAD),
            "a maybe is a lead: {text}"
        );
        assert!(!text.contains(region_pack::DIRECTIVE));
        assert_eq!(finished.record.tier, Some("low"));
        assert_eq!(finished.record.pack_parts, 1);
        assert_eq!(finished.record.pack_bytes, text.len());
    }

    #[test]
    fn a_weak_prompt_in_the_low_tier_should_get_the_compact_maybe_and_no_concept_search() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.concept = Ok(found(vec![hit("src/login.ts", 3)]));
        let log = Arc::clone(&fake.log);
        let finished = briefed_with("how does the login flow work", fake, no_verdict, model_low);
        let text = finished.text.unwrap();
        assert!(text.ends_with(LOW_CONFIDENCE), "{text}");
        assert!(!text.contains("src/login.ts"), "{text}");
        assert!(!text.contains("kind:"), "{text}");
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("concept")),
            "{:?}",
            log.lock().unwrap()
        );
        assert_eq!(finished.record.signal, Some("weak"));
    }

    #[test]
    fn a_low_tier_prompt_with_no_file_to_name_should_render_nothing() {
        let mut fake = Fake::new();
        let mut answer = on_topic_answer();
        answer.input.cofiles.clear();
        answer.lines.clear();
        fake.relevance = Ok(answer);
        let finished = briefed_with(PROSE, fake, no_verdict, model_low);
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.tier, Some("low"));
    }

    fn low_brief(files: Vec<RichHit>) -> Brief {
        Brief {
            signal: Some(Signal::Prose),
            relevance: Some(verdict_in(
                Tier::Low,
                &GateInput {
                    relevance: &on_topic_answer().input,
                    typed: PROSE,
                },
            )),
            confidence: Some(LOW_CONFIDENCE.to_string()),
            answered: 1,
            files,
            ..Brief::default()
        }
    }

    #[test]
    fn render_low_should_name_at_most_three_files_and_the_maybe_line() {
        let brief = low_brief(vec![
            rhit("a.rs", 1, "one"),
            rhit_no_text("b.rs", 0),
            rhit("c.rs", 3, "three"),
            rhit("d.rs", 4, "four"),
        ]);
        assert_eq!(
            render(&brief).unwrap(),
            [
                "[PIXEL:BRIEF]",
                "files: a.rs:1 — one; b.rs; c.rs:3 — three",
                LOW_CONFIDENCE,
            ]
            .join("\n")
        );
        assert_eq!(render_low(&low_brief(Vec::new())), None);
    }

    #[test]
    fn render_low_should_shorten_texts_before_it_drops_a_file_and_stay_within_its_cap() {
        let long = |name: &str| {
            rhit(
                &format!("{}/{name}.rs", "d".repeat(100)),
                7,
                &"t".repeat(300),
            )
        };
        let three = low_brief(vec![long("a"), long("b"), long("c")]);
        let text = render_low(&three).unwrap();
        // 3 x (105-char path + 120-char text) does not fit; half the text does.
        assert!(text.len() <= LOW_BRIEF_BYTES, "{}", text.len());
        assert_eq!(text.matches(".rs:7").count(), 3, "{text}");
        assert!(text.contains(&"t".repeat(60)), "{text}");
        assert!(!text.contains(&"t".repeat(61)), "{text}");
        assert!(text.ends_with(LOW_CONFIDENCE), "{text}");
        // Wide characters make paths alone too long for three, then two.
        let wide = |name: &str| rhit_no_text(&format!("{}{name}", "日".repeat(118)), 3);
        let text = render_low(&low_brief(vec![wide("a"), wide("b"), wide("c")])).unwrap();
        assert!(text.len() <= LOW_BRIEF_BYTES, "{}", text.len());
        assert_eq!(text.matches('日').count(), 118, "one file left: {text}");
        // A single file whose path and text are both made of 4-byte characters
        // loses its text last.
        let one = low_brief(vec![rhit(&"😀".repeat(118), 9, &"😀".repeat(900))]);
        let text = render_low(&one).unwrap();
        assert!(text.len() <= LOW_BRIEF_BYTES, "{}", text.len());
        let files_line = text.lines().nth(1).unwrap();
        assert!(!files_line.contains(" — "), "{text}");
        assert!(
            files_line.ends_with(&format!("{}:9", "😀".repeat(118))),
            "{text}"
        );
        assert!(text.ends_with(LOW_CONFIDENCE), "{text}");
        assert_eq!(LOW_BRIEF_BYTES, 700);
        assert_eq!(LOW_FILES, 3);
    }

    #[test]
    fn a_brief_without_a_relevance_decision_should_have_no_tier() {
        assert_eq!(Brief::default().tier(), None);
        assert_eq!(low_brief(Vec::new()).tier(), Some(Tier::Low));
    }

    #[test]
    fn gather_should_score_the_relevance_answer_without_waiting_for_the_meaning_search() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        fake.meaning_pause = Duration::from_millis(400);
        let started = Instant::now();
        let mut scored_after = None;
        let got = gather(
            PROSE,
            &fake,
            &no_verdict,
            model_high,
            &Mutex::new(Brief::default()),
            started + SECOND,
            &mut |got| {
                if got.scored.is_some() && scored_after.is_none() {
                    scored_after = Some(started.elapsed());
                }
                false
            },
        );
        assert!(
            scored_after.is_some_and(|after| after < Duration::from_millis(300)),
            "{scored_after:?}"
        );
        assert!(matches!(got.meaning, Some(Ok(_))));
        assert_eq!(got.scored.unwrap().tier, Tier::High);
    }

    #[test]
    fn gather_should_leave_a_prompt_unscored_when_the_relevance_probe_failed() {
        let mut fake = Fake::new();
        fake.relevance = Err("cold".into());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "x")]);
        let got = gather(
            PROSE,
            &fake,
            &no_verdict,
            model_high,
            &Mutex::new(Brief::default()),
            Instant::now() + SECOND,
            &mut |_| false,
        );
        assert!(got.scored.is_none());
        assert!(matches!(got.relevance, Some(Err(_))));
    }

    #[test]
    fn a_weak_prompt_the_model_does_not_apply_to_should_keep_its_pre_gate_brief() {
        // No code graph: the model says nothing, so the brief is the one the
        // probes' files make, with no tier claimed and no confidence line.
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let finished = briefed_with(
            "how does the login flow work",
            fake,
            no_verdict,
            model_no_graph,
        );
        let text = finished.text.unwrap();
        assert!(text.contains(&format!("{DAEMON}:280")), "{text}");
        assert!(
            text.contains("\nkind: lookup\n"),
            "the full route ran: {text}"
        );
        assert!(!text.contains("confidence"), "{text}");
        assert_eq!(finished.record.gate, "unjudged");
        assert_eq!(finished.record.tier, None);
        assert_eq!(finished.record.score, None);
    }

    #[test]
    fn a_prose_prompt_the_model_does_not_apply_to_should_get_no_brief() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        let finished = briefed_with(PROSE, fake, no_verdict, model_no_graph);
        assert_eq!(finished.text, None);
        assert_eq!(finished.record.gate, "closed");
        assert_eq!(
            finished.record.reason.as_deref(),
            Some("no code graph: the model does not apply")
        );
    }

    #[test]
    fn off_topic_reason_should_say_what_the_model_saw() {
        let mut verdict = verdict_in(
            Tier::Off,
            &GateInput {
                relevance: &off_topic_answer().input,
                typed: PROSE,
            },
        );
        assert_eq!(
            off_topic_reason(&verdict),
            "off topic: score -1.00, 3/3 key terms in the best file"
        );
        verdict.basis = Basis::NoKeywords;
        verdict.score = None;
        assert_eq!(off_topic_reason(&verdict), "off topic: no keyword to weigh");
        verdict.basis = Basis::NoGraph;
        assert_eq!(
            off_topic_reason(&verdict),
            "no code graph: the model does not apply"
        );
    }

    #[test]
    fn fold_should_open_the_admission_for_both_tiers_and_record_an_off_tier_without_silencing() {
        let plan = Plan::from_typed(PROSE, false, None);
        for (tier, open) in [(Tier::High, true), (Tier::Low, true), (Tier::Off, false)] {
            let state = Mutex::new(Brief::default());
            let got = gathered(Some(Ok(on_topic_answer())), None, tier);
            fold(&state, &plan, &got);
            let brief = edit(&state, |brief| brief.clone());
            assert_eq!(brief.admission == Admission::Open, open, "{tier:?}");
            assert!(!brief.silenced, "{tier:?}: fold never silences");
            assert_eq!(brief.tier(), Some(tier));
            assert_eq!(brief.confidence.is_some(), open, "{tier:?}");
        }
    }

    #[test]
    fn fold_should_open_a_covered_prompt_instead_of_admitting_it_by_kind() {
        // A walkthrough that shares enough words to pass its own bypass is
        // still an open brief: its answered evidence renders, where a Kind
        // admission without a pack would print nothing.
        let plan = Plan::from_typed(WALK, false, None);
        let state = Mutex::new(Brief::default());
        let got = off_covered(2, 4, 4);
        fold(&state, &plan, &got);
        let brief = edit(&state, |brief| brief.clone());
        assert_eq!(brief.admission, Admission::Open);
        assert!(!brief.silenced);
    }

    #[test]
    fn a_decision_should_name_the_model_that_made_it() {
        let finished = briefed(PROSE, Fake::new(), no_verdict);
        assert_eq!(
            finished.record.model,
            crate::execution_brief::gate_model::SOURCE
        );
        let declined = declined_record(Declined::NotAboutCode, "hello there");
        assert_eq!(declined.model, crate::execution_brief::gate_model::SOURCE);
        assert_eq!(declined.tier, None);
    }

    /// A daemon that counts launches, for the tests of the starter.
    struct Counting {
        probe: crate::DaemonProbe,
        launches: std::sync::atomic::AtomicUsize,
        stall: Duration,
    }

    impl Counting {
        fn new(probe: crate::DaemonProbe, stall: Duration) -> Arc<Self> {
            Arc::new(Self {
                probe,
                launches: std::sync::atomic::AtomicUsize::new(0),
                stall,
            })
        }

        fn launched(&self) -> usize {
            self.launches.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl autostart::Daemons for Counting {
        fn probe(&self, _: &Path) -> crate::DaemonProbe {
            std::thread::sleep(self.stall);
            self.probe
        }

        fn retire(&self, _: &Path) {}

        fn launch(&self, _: &Path) -> std::io::Result<()> {
            self.launches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// The starter a prompt gives `daemons`, with every switch on.
    fn starter_of(daemons: &Arc<Counting>) -> Starter {
        let daemons = Arc::clone(daemons);
        Box::new(move || {
            autostart::kick(
                Path::new("/repo"),
                autostart::Mode::Start,
                autostart::Permit {
                    brief: true,
                    indexed: true,
                    auto_start: true,
                },
                daemons,
            )
        })
    }

    fn with_starter(
        prompt: &str,
        gate: Gate,
        window: Duration,
        log: Option<PathBuf>,
        starter: Starter,
    ) -> Result<Pending, Declined> {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        try_start_with(
            prompt,
            gate,
            window,
            move |_| Box::new(fake),
            no_verdict,
            model_high,
            Around {
                log,
                starter: Some(starter),
                ..Around::default()
            },
        )
    }

    #[test]
    fn a_briefed_prompt_should_start_an_absent_daemon_and_log_that_it_did() {
        let dir = scratch_dir("starter-absent");
        let path = dir.join(decision_log::LOG_FILE);
        let daemons = Counting::new(crate::DaemonProbe::Absent, Duration::ZERO);
        let finished = with_starter(
            PROSE,
            OPEN,
            SECOND,
            Some(path.clone()),
            starter_of(&daemons),
        )
        .unwrap()
        .finish_with_record();
        assert_eq!(daemons.launched(), 1);
        assert_eq!(finished.record.daemon, Some("launched"));
        let logged: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(logged["daemon"], "launched");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_briefed_prompt_should_leave_a_current_daemon_alone_and_log_that_one_ran() {
        let daemons = Counting::new(crate::DaemonProbe::Current, Duration::ZERO);
        let finished = with_starter(PROSE, OPEN, SECOND, None, starter_of(&daemons))
            .unwrap()
            .finish_with_record();
        assert_eq!(daemons.launched(), 0);
        assert_eq!(finished.record.daemon, Some("running"));
    }

    #[test]
    fn a_prompt_that_is_not_briefed_should_start_no_daemon() {
        let closed = Gate {
            enabled: false,
            indexed: true,
        };
        let unindexed = Gate {
            enabled: true,
            indexed: false,
        };
        let cases = [
            ("callers of `fetchUser`", closed),
            ("callers of `fetchUser`", unindexed),
            ("ok", OPEN),
            ("thanks, that works", OPEN),
        ];
        for (prompt, gate) in cases {
            let daemons = Counting::new(crate::DaemonProbe::Absent, Duration::ZERO);
            let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = Arc::clone(&starts);
            let inner = starter_of(&daemons);
            let starter: Starter = Box::new(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                inner()
            });
            assert!(
                with_starter(prompt, gate, SECOND, None, starter).is_err(),
                "{prompt:?}"
            );
            assert_eq!(
                starts.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{prompt:?}: the starter ran"
            );
            assert_eq!(daemons.launched(), 0, "{prompt:?}");
        }
    }

    #[test]
    fn a_start_that_hangs_should_not_hold_the_brief_past_its_bound() {
        let daemons = Counting::new(crate::DaemonProbe::Absent, Duration::from_secs(30));
        let window = Duration::from_millis(200);
        let started = Instant::now();
        let finished = with_starter(PROSE, OPEN, window, None, starter_of(&daemons))
            .unwrap()
            .finish_with_record();
        let held = started.elapsed();
        assert_eq!(finished.record.daemon, None, "the outcome was not known");
        assert!(
            held < window + autostart::HOOK_BOUND + Duration::from_secs(2),
            "the brief was held for {held:?}"
        );
        assert!(
            held >= autostart::HOOK_BOUND,
            "the hook gave the start its bound before it gave up: {held:?}"
        );
    }

    #[test]
    fn a_decision_should_name_where_its_evidence_came_from() {
        let finished = briefed(PROSE, Fake::new(), no_verdict);
        assert_eq!(finished.record.route, Some("unknown"));
        assert_eq!(finished.record.daemon, None, "no starter was given");
        let declined = declined_record(Declined::NotAboutCode, "hello there");
        assert_eq!((declined.route, declined.daemon), (None, None));
    }

    /// A daemon source file the fake can be asked for lines of.
    fn daemon_source() -> Vec<String> {
        let mut lines: Vec<String> = (1..=278)
            .map(|n| format!("let filler_{n} = {n};"))
            .collect();
        lines.extend(
            [
                "/// Wait until the daemon answers its socket.",
                "pub fn watch_ready(root: &Path) -> bool {",
                "    let mut waited = 0;",
                "    while waited < READY_TRIES {",
                "        waited += 1;",
                "    }",
                "    waited < READY_TRIES",
                "}",
            ]
            .map(String::from),
        );
        lines
    }

    /// A weakly code-shaped prompt the high-tier model accepts, with the
    /// relevance probe, a meaning search and the daemon source answering.
    fn confident_fake() -> Fake {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 280, "pub fn watch_ready")]);
        fake.sources = vec![(DAEMON.to_string(), daemon_source())];
        fake
    }

    const WEAK: &str = "how does the daemon handle changes made during startup";

    #[test]
    fn the_receipt_should_state_the_searches_that_ran_with_their_counts() {
        let finished = briefed(WEAK, confident_fake(), no_verdict);
        let text = finished.text.unwrap();
        assert!(
            text.contains(
                "searched: content+symbols+paths for daemon, changes, startup (3 terms, 1000 files) · meaning search returned 1 chunks\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("result: the matches below are the best across both searches;"),
            "{text}"
        );
        assert!(finished.record.receipt);
    }

    #[test]
    fn the_receipt_should_never_name_a_meaning_search_that_did_not_answer() {
        let mut fake = confident_fake();
        fake.meaning = Err("cold".into());
        let finished = briefed(WEAK, fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(
            text.contains("searched: content+symbols+paths for"),
            "{text}"
        );
        assert!(!text.contains("meaning search"), "{text}");
        assert!(text.contains("across the search;"), "{text}");
    }

    #[test]
    fn a_brief_without_a_relevance_answer_should_carry_no_receipt() {
        let mut fake = confident_fake();
        fake.relevance = Err("cold".into());
        let finished = briefed(WEAK, fake, no_verdict);
        let text = finished.text.unwrap_or_default();
        assert!(!text.contains("searched:"), "{text}");
        assert!(!finished.record.receipt);
    }

    #[test]
    fn a_low_tier_brief_should_carry_neither_receipt_nor_excerpt() {
        let finished = briefed_with(WEAK, confident_fake(), no_verdict, model_low);
        let text = finished.text.unwrap();
        assert!(
            !text.contains("searched:") && !text.contains("\nanswer "),
            "{text}"
        );
        assert!(!finished.record.receipt && !finished.record.answer);
    }

    #[test]
    fn a_strong_prompt_should_carry_neither_receipt_nor_excerpt() {
        let mut fake = confident_fake();
        fake.symbols = Ok(vec![symbol(DAEMON, "watch_ready")]);
        let finished = briefed("what does `watch_ready` do", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(
            !text.contains("searched:") && !text.contains("\nanswer "),
            "{text}"
        );
        assert!(text.len() <= BRIEF_BYTES);
    }

    #[test]
    fn the_excerpt_should_hold_the_signature_the_doc_line_and_the_body_within_the_cap() {
        let finished = briefed(WEAK, confident_fake(), no_verdict);
        let text = finished.text.unwrap();
        // The shipped build has no validated ZERO rule: the excerpt is not
        // shown, it fixes the declaration to read.
        assert!(!text.contains("\nanswer "), "{text}");
        assert!(
            text.contains("\nread: crates/pixel-daemon/src/daemon.rs:279-286 — watch_ready\n"),
            "{text}"
        );
        assert!(text.ends_with(READ_LINE), "{text}");
        assert!(text.contains(MEDIUM_CONFIDENCE), "{text}");
        assert!(text.len() <= PROSE_BRIEF_BYTES, "{} bytes", text.len());
        assert!(!finished.record.answer);
        assert_eq!(finished.record.excerpt_bytes, 0);
        assert_eq!(finished.record.bytes, text.len());
    }

    #[test]
    fn a_source_that_cannot_be_read_should_leave_the_brief_without_an_excerpt() {
        let mut fake = confident_fake();
        fake.sources.clear();
        let finished = briefed(WEAK, fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(!text.contains("\nanswer "), "{text}");
        assert!(text.contains("searched:"), "{text}");
        assert!(!finished.record.answer);
        assert_eq!(finished.record.excerpt_bytes, 0);
    }

    #[test]
    fn a_credential_shaped_file_should_never_be_excerpted() {
        let mut fake = confident_fake();
        fake.relevance = Ok(RelevanceAnswer {
            lines: vec![rhit("deploy/.env", 2, "TOKEN=x")],
            ..on_topic_answer()
        });
        fake.meaning = Ok(Vec::new());
        fake.sources = vec![(
            "deploy/.env".to_string(),
            vec!["TOKEN=x".into(), "B=2".into()],
        )];
        let finished = briefed(WEAK, fake, no_verdict);
        let text = finished.text.unwrap_or_default();
        assert!(
            !text.contains("\nanswer ") && !text.contains("TOKEN"),
            "{text}"
        );
    }

    /// [`answer_evidence`] on `brief` against `fake`.
    fn answered(brief: Brief, fake: &Fake) -> Brief {
        let state = Mutex::new(brief);
        answer_evidence(fake, &state, Instant::now() + SECOND);
        state.into_inner().unwrap()
    }

    #[test]
    fn a_flow_brief_should_list_the_hops_with_their_sites() {
        let mut fake = Fake::new();
        fake.symbols = Ok(vec![symbol("src/a.rs", "render_page")]);
        fake.source = Some("pub fn render_page() {".into());
        let mut brief = high_brief();
        brief.kind = Some(QuestionKind::Flow);
        brief.flow_hops = vec![
            "render_page".into(),
            "middleware".into(),
            "send_response".into(),
        ];
        let brief = answered(brief, &fake);
        assert_eq!(brief.excerpts.len(), 1);
        assert_eq!(brief.excerpts[0].label, "flow");
        assert_eq!(
            brief.excerpts[0].lines[0],
            "src/a.rs:3 render_page — pub fn render_page() {"
        );
        assert_eq!(brief.excerpts[0].lines.len(), 3);
        // The extra lookups are outside the coverage count.
        assert_eq!((brief.ops, brief.answered), (3, 3));
    }

    #[test]
    fn a_flow_brief_whose_hops_do_not_resolve_should_fall_back_to_the_generic_excerpt() {
        let mut fake = Fake::new();
        fake.symbols = Ok(Vec::new());
        fake.sources = vec![(DAEMON.to_string(), daemon_source())];
        let mut brief = high_brief();
        brief.kind = Some(QuestionKind::Flow);
        brief.flow_hops = vec!["a".into(), "b".into()];
        brief.files = vec![rhit(DAEMON, 280, "pub fn watch_ready")];
        let brief = answered(brief, &fake);
        assert_eq!(brief.excerpts.len(), 1);
        assert_eq!(brief.excerpts[0].label, format!("{DAEMON}:280"));
    }

    #[test]
    fn a_tests_brief_should_name_the_tests_with_their_first_assert() {
        let mut fake = Fake::new();
        fake.sources = vec![(
            "tests/ready.rs".to_string(),
            [
                "#[test]",
                "fn ready_after_startup() {",
                "    let up = watch_ready(root);",
                "    assert!(up);",
                "}",
            ]
            .map(String::from)
            .to_vec(),
        )];
        let mut brief = high_brief();
        brief.kind = Some(QuestionKind::Tests);
        brief.defined = vec![symbol("src/a.rs", "watch_ready")];
        brief.tests = vec!["tests/ready.rs".into()];
        let brief = answered(brief, &fake);
        assert_eq!(brief.excerpts.len(), 1);
        assert_eq!(brief.excerpts[0].label, "tests");
        assert_eq!(
            brief.excerpts[0].lines,
            ["tests/ready.rs:2 ready_after_startup — assert!(up);"]
        );
    }

    #[test]
    fn a_config_brief_should_show_the_setting_line_with_its_default_and_where_it_is_read() {
        let mut fake = Fake::new();
        fake.sources = vec![(
            "src/cfg.rs".to_string(),
            vec![
                "/// Tries before the daemon is called late.".into(),
                "pub const READY_TRIES: u32 = 40;".into(),
            ],
        )];
        let mut brief = high_brief();
        brief.kind = Some(QuestionKind::Config);
        brief.receipt = Receipt::new(
            &RelevanceInput {
                keywords: vec![keyword("tries", 2.0)],
                ..on_topic_answer().input
            },
            None,
        );
        brief.files = vec![
            rhit("src/cfg.rs", 2, "pub const READY_TRIES: u32 = 40;"),
            rhit("src/run.rs", 9, "let n = std::env::var(READY_TRIES).ok();"),
            rhit("src/other.rs", 1, "pub const UNRELATED_LIMIT: u32 = 1;"),
        ];
        let brief = answered(brief, &fake);
        assert_eq!(brief.excerpts.len(), 2);
        assert_eq!(brief.excerpts[0].label, "config");
        assert_eq!(
            brief.excerpts[0].lines,
            [
                "src/cfg.rs:2 — pub const READY_TRIES: u32 = 40;",
                "src/run.rs:9 — let n = std::env::var(READY_TRIES).ok();"
            ]
        );
        // The generic excerpt of the best file follows the kind's block.
        assert_eq!(brief.excerpts[1].label, "src/cfg.rs:2");
        assert_eq!(
            brief.excerpts[1].lines[0],
            "1| /// Tries before the daemon is called late."
        );
    }

    #[test]
    fn answer_evidence_should_skip_a_rationale_a_maybe_a_strong_prompt_and_a_gap() {
        let mut fake = Fake::new();
        fake.sources = vec![(DAEMON.to_string(), daemon_source())];
        let ready = || {
            let mut brief = high_brief();
            brief.files = vec![rhit(DAEMON, 280, "pub fn watch_ready")];
            brief.excerpts.clear();
            brief
        };
        assert_eq!(answered(ready(), &fake).excerpts.len(), 1);
        let mut rationale = ready();
        rationale.kind = Some(QuestionKind::Rationale);
        let mut strong = ready();
        strong.signal = Some(Signal::Strong);
        let mut cut = ready();
        cut.cut = true;
        // A maybe gathers its chunks too, only to fix the ranges to read.
        let mut low = ready();
        low.relevance = low.relevance.map(|verdict| relevance::Verdict {
            tier: Tier::Low,
            ..verdict
        });
        assert_eq!(answered(low, &fake).excerpts.len(), 1);
        for (name, brief) in [("rationale", rationale), ("strong", strong), ("cut", cut)] {
            assert!(answered(brief, &fake).excerpts.is_empty(), "{name}");
        }
    }

    #[test]
    fn the_generic_excerpt_should_prefer_code_over_tests_that_quote_the_question() {
        for test in [
            "tests/a.rs",
            "src/a_test.rs",
            "web/a.test.ts",
            "src/tests.rs",
            "test_a.py",
        ] {
            assert!(answer::is_test_path(test), "{test}");
        }
        for code in ["src/a.rs", "src/latest.rs", "docs/contest.md"] {
            assert!(!answer::is_test_path(code), "{code}");
        }
        let mut fake = Fake::new();
        fake.sources = vec![
            ("tests/a.rs".to_string(), vec!["fn quoted() {}".into()]),
            ("src/b.rs".to_string(), vec!["fn b() {}".into()]),
            ("src/c.rs".to_string(), vec!["fn c() {}".into()]),
        ];
        let mut brief = high_brief();
        brief.files = vec![
            rhit("tests/a.rs", 1, "x"),
            rhit("src/b.rs", 1, "x"),
            rhit("src/c.rs", 1, "x"),
        ];
        let labels: Vec<String> = answered(brief, &fake)
            .excerpts
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(labels, ["src/b.rs:1", "src/c.rs:1"]);
    }

    #[test]
    fn the_generic_excerpt_should_cover_two_distinct_files_and_skip_the_packed_definition() {
        let mut fake = Fake::new();
        fake.sources = vec![
            (DAEMON.to_string(), daemon_source()),
            (
                "b.rs".to_string(),
                vec!["fn b() {".into(), "    go();".into(), "}".into()],
            ),
            ("c.rs".to_string(), vec!["fn c() {}".into()]),
        ];
        let mut brief = high_brief();
        brief.files = vec![
            rhit(DAEMON, 280, "x"),
            rhit(DAEMON, 281, "x"),
            rhit("b.rs", 1, "x"),
            rhit("c.rs", 1, "x"),
        ];
        let kept = answered(brief.clone(), &fake);
        let labels: Vec<&str> = kept.excerpts.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            [format!("{DAEMON}:280").as_str(), "b.rs:1", "c.rs:1"]
        );
        brief.defined = vec![symbol(DAEMON, "watch_ready")];
        brief.def_body = Some("fn watch_ready() {}".into());
        let packed = answered(brief, &fake);
        let labels: Vec<&str> = packed.excerpts.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["b.rs:1", "c.rs:1"]);
    }

    #[test]
    fn a_rationale_brief_should_carry_the_receipt_and_no_excerpt() {
        let mut fake = confident_fake();
        fake.status = Ok(StatusProbe {
            facts_fresh: Some(true),
        });
        fake.history = Ok((vec![history("abc1234", "retry on startup")], Vec::new()));
        let finished = briefed("why does the daemon retry during startup", fake, no_verdict);
        let text = finished.text.unwrap();
        assert!(text.contains("searched:"), "{text}");
        assert!(!text.contains("\nanswer "), "{text}");
    }

    fn high_brief() -> Brief {
        let hit = rhit("src/a.rs", 3, "fn a");
        let model = verdict_in(
            Tier::High,
            &GateInput {
                relevance: &on_topic_answer().input,
                typed: "t",
            },
        );
        Brief {
            signal: Some(Signal::Prose),
            relevance: Some(model),
            receipt: Receipt::new(&on_topic_answer().input, Some(2)),
            files: vec![hit.clone(); 6],
            excerpts: (0..2)
                .map(|n| Excerpt {
                    label: format!("src/f{n}.rs:3"),
                    path: format!("src/f{n}.rs"),
                    from_meaning: true,
                    meaning_rank: Some(n),
                    density: 3,
                    lines: (0..10)
                        .map(|line| format!("{line}| {}", "x".repeat(105)))
                        .collect(),
                })
                .collect(),
            ops: 3,
            answered: 3,
            finished: true,
            ..Brief::default()
        }
    }

    #[test]
    fn an_oversized_brief_should_drop_the_second_excerpt_first_and_keep_the_receipt() {
        let mut brief = high_brief();
        brief.caps = vec!["c".repeat(110); 10];
        // The ZERO step fires, so the excerpts are shown.
        brief.zero_rule = Some((0, 0, 2));
        brief.lexical = vec!["src/f0.rs".into()];
        let (text, stats) = fit(&brief).unwrap();
        assert!(text.len() <= PROSE_BRIEF_BYTES, "{}", text.len());
        assert!(
            text.contains("searched:") && text.contains("result:"),
            "{text}"
        );
        assert!(stats.receipt && stats.answer, "{stats:?}");
        let count = |label: &str| {
            text.lines()
                .skip_while(|line| !line.starts_with(&format!("answer {label}")))
                .skip(1)
                .take_while(|line| line.starts_with("  "))
                .count()
        };
        let (first, second) = (count("src/f0.rs:3"), count("src/f1.rs:3"));
        // Whatever had to go went from the second block before the first.
        assert!(second < 10, "{text}");
        assert!(second == 0 || first == 10, "{text}");
    }

    #[test]
    fn a_tight_cap_should_empty_the_excerpts_before_it_touches_the_receipt() {
        let mut brief = high_brief();
        brief.files = (0..4)
            .map(|n| rhit(&format!("crates/long/{n}.rs"), 1, &"t".repeat(110)))
            .collect();
        brief.def_body = Some("b".repeat(480));
        brief.defined = vec![symbol("src/handle.ts", "handle")];
        brief.callers = (0..10)
            .map(|n| caller("p.rs", &format!("via{n}"), n))
            .collect();
        let (text, stats) = fit(&brief).unwrap();
        assert!(stats.receipt, "{text}");
        assert!(text.contains("result: the matches below"), "{text}");
        if !stats.answer {
            assert_eq!(stats.excerpt_bytes, 0);
            assert!(!text.contains("\nanswer "), "{text}");
        }
    }

    #[test]
    fn a_cut_packet_should_carry_no_receipt_and_no_excerpt() {
        let mut brief = high_brief();
        brief.cut = true;
        let (text, stats) = fit(&brief).unwrap();
        assert!(
            !text.contains("searched:") && !text.contains("\nanswer "),
            "{text}"
        );
        assert_eq!(stats, AnswerStats::default());
    }

    fn lead_at(path: &str, line: u32, symbol: Option<&str>) -> MeaningHit {
        MeaningHit {
            symbol: symbol.map(String::from),
            ..lead(path, line, "chunk")
        }
    }

    fn two_file_fake() -> Fake {
        let mut fake = Fake::new();
        fake.sources = vec![
            (DAEMON.to_string(), daemon_source()),
            (
                "crates/pixel/src/other.rs".to_string(),
                vec!["fn other() {".into(), "    go();".into(), "}".into()],
            ),
        ];
        fake
    }

    #[test]
    fn the_excerpt_should_come_from_the_meaning_chunk_not_the_lexical_line() {
        let mut brief = high_brief();
        brief.leads = vec![lead_at(DAEMON, 280, None)];
        brief.files = vec![
            rhit(DAEMON, 3, "filler"),
            rhit("crates/pixel/src/other.rs", 1, "go"),
        ];
        let brief = answered(brief, &two_file_fake());
        assert_eq!(brief.excerpts[0].label, format!("{DAEMON}:280"));
        assert!(brief.excerpts[0].from_meaning);
        let other = brief
            .excerpts
            .iter()
            .find(|excerpt| excerpt.path.ends_with("other.rs"))
            .expect("the second file speaks too");
        assert_eq!(other.label, "crates/pixel/src/other.rs:1");
        assert!(!other.from_meaning);
    }

    #[test]
    fn the_file_ranked_first_should_speak_first_even_against_a_better_scored_chunk() {
        let mut brief = high_brief();
        brief.leads = vec![lead_at(DAEMON, 280, None)];
        brief.files = vec![
            rhit("crates/pixel/src/other.rs", 1, "go"),
            rhit(DAEMON, 280, "x"),
        ];
        let brief = answered(brief, &two_file_fake());
        assert_eq!(brief.excerpts[0].label, "crates/pixel/src/other.rs:1");
    }

    #[test]
    fn excerpts_should_skip_tests_docs_and_test_modules_unless_the_question_asks() {
        let mut fake = two_file_fake();
        let mut inside: Vec<String> = vec![
            "fn real() {}".into(),
            "#[cfg(test)]".into(),
            "mod tests {".into(),
        ];
        inside.extend((0..15).map(|n| format!("    fn t{n}() {{}}")));
        fake.sources
            .push(("src/mod_with_tests.rs".to_string(), inside));
        for path in [
            "tests/a.rs",
            "docs/guide.md",
            "CHANGELOG.md",
            "src/lib_test.rs",
        ] {
            fake.sources
                .push((path.to_string(), vec!["fn x() {}".into()]));
        }
        let mut brief = high_brief();
        brief.leads = vec![
            lead_at("tests/a.rs", 1, None),
            lead_at("docs/guide.md", 1, None),
            lead_at("CHANGELOG.md", 1, None),
            lead_at("src/lib_test.rs", 1, None),
            lead_at("crates/pixel/src/other.rs", 17, Some("render_should_wrap")),
            lead_at("src/mod_with_tests.rs", 10, None),
            lead_at(DAEMON, 280, Some("watch_ready")),
        ];
        brief.files.clear();
        let kept = answered(brief.clone(), &fake);
        let paths: Vec<&str> = kept.excerpts.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, [DAEMON]);
        // A question about tests or docs may cut from them.
        brief.kind = Some(QuestionKind::Tests);
        brief.defined = Vec::new();
        let tests = answered(brief, &fake);
        assert!(
            tests.excerpts.iter().any(|e| e.path == "tests/a.rs"),
            "{:?}",
            tests.excerpts
        );
    }

    /// A high-tier brief whose top chunk is `DAEMON:280`: the meaning
    /// search's best, holding `density` keywords, and the lexical probe's
    /// co-files are `lexical`. The ZERO rule `(0, 0, 2)` is in force.
    fn zeroing_brief(lexical: &[&str], density: usize) -> Brief {
        let mut brief = high_brief();
        brief.excerpts = vec![Excerpt {
            label: format!("{DAEMON}:280"),
            path: DAEMON.to_string(),
            from_meaning: true,
            meaning_rank: Some(0),
            density,
            lines: vec!["280| pub fn watch_ready()".into()],
        }];
        brief.reads = vec![ReadRange {
            path: DAEMON.to_string(),
            first: 280,
            last: 288,
            symbol: Some("watch_ready".to_string()),
        }];
        brief.lexical = lexical.iter().map(ToString::to_string).collect();
        brief.confidence = Some(HIGH_CONFIDENCE.to_string());
        brief.zero_rule = Some((0, 0, 2));
        brief
    }

    #[test]
    fn the_zero_step_should_fire_only_when_the_whole_rule_holds() {
        let zero = zeroing_brief(&[DAEMON, "b.rs"], 2);
        assert!(zero.zero() && zero.confident());
        let (text, stats) = fit(&zero).unwrap();
        assert!(text.contains(HIGH_CONFIDENCE), "{text}");
        assert!(
            text.contains(&format!("\nanswer {DAEMON}:280:\n")),
            "{text}"
        );
        assert!(text.ends_with(ZERO_LINE), "{text}");
        assert!(!text.contains("read: "), "{text}");
        assert!(text.contains("answer from them if they suffice"), "{text}");
        assert!(stats.answer);

        // One keyword short, the file second among the lexical co-files, a
        // lexical-only chunk, a lower meaning rank, no rule: each is a miss.
        let mut misses = vec![
            zeroing_brief(&[DAEMON], 1),
            zeroing_brief(&["b.rs", DAEMON], 2),
        ];
        let mut lexical = zeroing_brief(&[DAEMON], 2);
        lexical.excerpts[0].from_meaning = false;
        let mut ranked = zeroing_brief(&[DAEMON], 2);
        ranked.excerpts[0].meaning_rank = Some(1);
        let mut ruleless = zeroing_brief(&[DAEMON], 2);
        ruleless.zero_rule = None;
        let mut noted = zeroing_brief(&[DAEMON], 2);
        noted.unresolved = vec!["find-symbol x: 2 candidates".into()];
        misses.extend([lexical, ranked, ruleless, noted]);
        for (index, brief) in misses.iter().enumerate() {
            assert!(!brief.zero(), "miss {index}");
            let (text, _) = fit(brief).unwrap();
            assert!(!text.contains(ZERO_LINE), "miss {index}: {text}");
            assert!(!text.contains("\nanswer "), "miss {index}: {text}");
            assert!(!text.contains("confidence: high"), "miss {index}: {text}");
            assert!(text.ends_with(READ_LINE), "miss {index}: {text}");
        }
    }

    #[test]
    fn no_zero_rule_should_ship_until_one_is_validated() {
        // Dev split of eval/brief-gate/answer_spans.jsonl: the strict rule
        // fired once, the loosest five times and was right twice.
        assert!(answer::ZERO_RULE.is_none());
        assert!(Brief::default().zero_rule.is_none());
        assert!(answer::zero_enabled() || std::env::var(answer::ZERO_ENV).is_ok());
    }

    #[test]
    fn a_brief_that_did_not_earn_zero_should_end_on_the_ranges_to_read() {
        let mut brief = zeroing_brief(&["b.rs"], 2);
        brief.reads.push(ReadRange {
            path: "crates/pixel/src/other.rs".into(),
            first: 1,
            last: 40,
            symbol: None,
        });
        let (text, stats) = fit(&brief).unwrap();
        assert!(text.contains(MEDIUM_CONFIDENCE), "{text}");
        assert!(!text.contains("\nanswer "), "{text}");
        assert!(!stats.answer);
        let tail: Vec<&str> = text.lines().rev().take(3).collect();
        assert_eq!(
            tail,
            [
                READ_LINE,
                "also: crates/pixel/src/other.rs:1-40",
                &format!("read: {DAEMON}:280-288 — watch_ready"),
            ]
        );
        assert_eq!(text.matches("read: ").count(), 1, "{text}");
        assert!(text.len() <= PROSE_BRIEF_BYTES);
    }

    #[test]
    fn a_partial_brief_should_name_its_gap_and_still_end_on_the_instruction() {
        let mut brief = zeroing_brief(&[DAEMON], 2);
        brief.unresolved = vec!["find-symbol x: 2 candidates".into()];
        brief.next = Some("pixel find-code 'x'".into());
        let (text, _) = fit(&brief).unwrap();
        assert!(text.contains("packet partial"), "{text}");
        assert!(text.contains("\nnext: pixel find-code 'x'\n"), "{text}");
        assert!(text.ends_with(READ_LINE), "{text}");
        assert!(!text.contains("answer from them"), "{text}");
    }

    #[test]
    fn no_range_should_be_named_for_a_cut_packet_or_a_strong_prompt_without_a_definition() {
        let mut strong = zeroing_brief(&[DAEMON], 2);
        strong.signal = Some(Signal::Strong);
        strong.reads.clear();
        let mut cut = zeroing_brief(&[DAEMON], 2);
        cut.cut = true;
        for brief in [strong, cut] {
            assert!(brief.read_ranges().is_empty());
            let text = fit(&brief).map(|(text, _)| text).unwrap_or_default();
            assert!(!text.contains("read: "), "{text}");
        }
        // Without a range the brief keeps a verification footer.
        let mut bare = zeroing_brief(&[DAEMON], 2);
        bare.reads.clear();
        bare.zero_rule = None;
        let (text, _) = fit(&bare).unwrap();
        assert!(text.contains(MEDIUM_FILES_CONFIDENCE), "{text}");
        assert!(text.ends_with(UNCORROBORATED_FOOTER), "{text}");
    }

    #[test]
    fn a_maybe_should_name_the_range_to_read_inside_its_cap() {
        let mut brief = low_brief(vec![rhit("a.rs", 1, "one"), rhit("b.rs", 2, "two")]);
        brief.finished = true;
        brief.reads = vec![ReadRange {
            path: "a.rs".into(),
            first: 10,
            last: 30,
            symbol: Some("run".into()),
        }];
        let (text, _) = fit(&brief).unwrap();
        assert!(text.contains("\nread: a.rs:10-30 — run\n"), "{text}");
        assert!(text.ends_with(READ_LINE), "{text}");
        assert!(text.contains(LOW_CONFIDENCE), "{text}");
        assert!(text.len() <= LOW_READ_BRIEF_BYTES, "{}", text.len());
    }

    #[test]
    fn a_flow_or_tests_question_should_borrow_the_symbol_of_the_best_non_test_chunk() {
        let mut brief = high_brief();
        brief.leads = vec![
            lead_at("tests/a.rs", 3, Some("a_test_symbol")),
            lead_at(DAEMON, 280, Some("watch_ready")),
        ];
        let state = Mutex::new(brief);
        let plan = |kind, text: &str| Plan {
            anchors: Anchors::from_text(text),
            change_intent: false,
            kind,
            typed: text.to_string(),
            concept: None,
        };
        let seeded_flow = seeded(plan(QuestionKind::Flow, "what calls the watchdog"), &state);
        assert_eq!(
            seeded_flow.anchors.symbol_name().as_deref(),
            Some("watch_ready")
        );
        // Another kind, or a question that names its own symbol, keeps its anchors.
        let lookup = seeded(plan(QuestionKind::Lookup, "what is the watchdog"), &state);
        assert!(lookup.anchors.names().next().is_none());
        let named = seeded(plan(QuestionKind::Flow, "who calls `retry_loop`"), &state);
        assert_eq!(named.anchors.names().collect::<Vec<_>>(), ["retry_loop"]);
        // A maybe never borrows one.
        state.lock().unwrap().relevance = None;
        let cold = seeded(
            plan(QuestionKind::Tests, "which tests cover the watchdog"),
            &state,
        );
        assert!(cold.anchors.names().next().is_none());
    }

    #[test]
    fn a_one_endpoint_flow_brief_should_list_the_callers_as_its_hops() {
        let mut fake = Fake::new();
        fake.source = Some("fn run() { watch_ready() }".into());
        let mut brief = high_brief();
        brief.kind = Some(QuestionKind::Flow);
        brief.callers = vec![
            caller("src/run.rs", "run", 12),
            caller("src/boot.rs", "boot", 7),
        ];
        let brief = answered(brief, &fake);
        assert_eq!(brief.excerpts[0].label, "callers");
        assert_eq!(
            brief.excerpts[0].lines,
            [
                "src/run.rs:12 run — fn run() { watch_ready() }",
                "src/boot.rs:7 boot — fn run() { watch_ready() }"
            ]
        );
    }

    fn files_line(text: &str) -> String {
        text.lines()
            .find(|line| line.starts_with("files: "))
            .unwrap_or_default()
            .to_string()
    }

    fn ranked_fake() -> Fake {
        let mut fake = Fake::new();
        fake.relevance = Ok(RelevanceAnswer {
            lines: vec![
                rhit("tests/a.rs", 1, "x"),
                rhit("docs/x.md", 1, "x"),
                rhit("src/b.rs", 1, "x"),
                rhit("eval/run.py", 1, "x"),
                rhit("src/c.rs", 1, "x"),
            ],
            ..on_topic_answer()
        });
        fake.meaning = Err("cold".into());
        fake
    }

    #[test]
    fn the_file_list_should_rank_tests_docs_and_eval_after_the_code_without_dropping_them() {
        let finished = briefed(WEAK, ranked_fake(), no_verdict);
        let files = files_line(&finished.text.unwrap());
        let at = |path: &str| {
            files
                .find(path)
                .unwrap_or_else(|| panic!("{path} in {files}"))
        };
        assert!(at("src/b.rs") < at("src/c.rs"), "{files}");
        for later in ["tests/a.rs", "docs/x.md", "eval/run.py"] {
            assert!(at("src/c.rs") < at(later), "{files}");
        }
        // The demoted keep their own order.
        assert!(at("tests/a.rs") < at("docs/x.md") && at("docs/x.md") < at("eval/run.py"));
    }

    #[test]
    fn a_question_about_tests_or_docs_should_keep_those_files_in_place() {
        let finished = briefed(
            "which tests cover how the daemon handles startup changes",
            ranked_fake(),
            no_verdict,
        );
        let files = files_line(&finished.text.unwrap());
        assert!(files.find("tests/a.rs") < files.find("src/b.rs"), "{files}");
        // Docs stay demoted when only tests were asked for.
        assert!(files.find("src/c.rs") < files.find("docs/x.md"), "{files}");
        let finished = briefed(
            "what do the docs say about how the daemon handles startup changes",
            ranked_fake(),
            no_verdict,
        );
        let files = files_line(&finished.text.unwrap());
        assert!(files.find("docs/x.md") < files.find("src/b.rs"), "{files}");
        assert!(files.find("src/c.rs") < files.find("tests/a.rs"), "{files}");
    }

    #[test]
    fn the_best_chunk_should_be_the_one_the_meaning_search_and_the_keywords_both_favour() {
        let mut source: Vec<String> = (1..=90).map(|n| format!("let filler_{n} = {n};")).collect();
        for (line, name) in [(10, "a_chunk"), (40, "b_chunk"), (70, "c_chunk")] {
            source[line - 1] = format!("fn {name}() {{");
        }
        source[44] = "    daemon.notice(changes_during_startup);".into();
        source[74] = "    daemon.notice(changes);".into();
        let mut fake = Fake::new();
        fake.sources = vec![(DAEMON.to_string(), source)];
        let mut brief = high_brief();
        brief.leads = [(10, 20), (40, 50), (70, 80)]
            .iter()
            .map(|&(start, end)| MeaningHit {
                end_line: end,
                ..lead_at(DAEMON, start, None)
            })
            .collect();
        brief.files = vec![rhit(DAEMON, 10, "x")];
        brief.excerpts.clear();
        let brief = answered(brief, &fake);
        let labels: Vec<&str> = brief.excerpts.iter().map(|e| e.label.as_str()).collect();
        // Meaning ranks a, b, c; keyword density ranks b, c, a: b wins, and
        // the file of the best chunk may show all three.
        assert_eq!(
            labels,
            [
                format!("{DAEMON}:40"),
                format!("{DAEMON}:10"),
                format!("{DAEMON}:70")
            ]
        );
        let first = brief.excerpts[0].lines.join("\n");
        assert!(first.contains("changes_during_startup"), "{first}");
        assert!(
            brief
                .excerpts
                .iter()
                .all(|e| e.lines.len() <= 2 + answer::CHUNK_BODY_LINES)
        );
    }

    #[test]
    fn only_the_file_of_the_best_chunk_may_show_more_than_one_chunk() {
        let mut fake = two_file_fake();
        fake.sources[1].1 = (1..=30).map(|n| format!("fn other_{n}() {{}}")).collect();
        let mut brief = high_brief();
        brief.leads = vec![
            lead_at(DAEMON, 280, None),
            lead_at("crates/pixel/src/other.rs", 2, None),
            lead_at("crates/pixel/src/other.rs", 20, None),
        ];
        brief.files = vec![
            rhit(DAEMON, 280, "x"),
            rhit("crates/pixel/src/other.rs", 2, "x"),
        ];
        brief.excerpts.clear();
        let brief = answered(brief, &fake);
        let other = brief
            .excerpts
            .iter()
            .filter(|e| e.path.ends_with("other.rs"))
            .count();
        assert_eq!(other, 1, "{:?}", brief.excerpts);
    }

    fn function_file() -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for (name, body) in [
            ("alpha", "unrelated work"),
            ("beta", "daemon startup changes"),
            ("gamma", "other things"),
        ] {
            lines.push(format!("/// Does {body}."));
            lines.push(format!("pub fn {name}() {{"));
            for n in 0..10 {
                lines.push(format!("    step_{name}_{n}(); // {body}"));
            }
            lines.push("}".into());
            lines.push(String::new());
        }
        lines
    }

    #[test]
    fn the_range_should_be_the_function_whose_lines_hold_the_most_probed_keywords() {
        let mut fake = Fake::new();
        fake.sources = vec![
            (DAEMON.to_string(), function_file()),
            ("crates/pixel/src/other.rs".to_string(), function_file()),
            ("docs/guide.md".to_string(), function_file()),
        ];
        let mut brief = high_brief();
        brief.files = vec![
            rhit("docs/guide.md", 1, "x"),
            rhit(DAEMON, 1, "x"),
            rhit(DAEMON, 20, "x"),
            rhit("crates/pixel/src/other.rs", 1, "x"),
        ];
        let terms: Vec<String> = ["daemon", "startup", "changes"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let reads = read_evidence(&fake, &brief, &terms, Instant::now() + SECOND);
        // The guide is demoted; one range per file; each the `beta` function.
        let paths: Vec<&str> = reads.iter().map(|read| read.path.as_str()).collect();
        assert_eq!(paths, [DAEMON, "crates/pixel/src/other.rs"]);
        for read in &reads {
            assert_eq!(read.symbol.as_deref(), Some("beta"), "{read:?}");
            assert!(read.first <= 15 && read.last >= 25, "{read:?}");
            assert!(read.last - read.first < 60);
        }
        // A question about docs keeps the guide.
        brief.wants_docs = true;
        let reads = read_evidence(&fake, &brief, &terms, Instant::now() + SECOND);
        assert_eq!(reads[0].path, "docs/guide.md");
    }

    #[test]
    fn a_strong_prompt_should_read_its_definition_then_its_callers() {
        let mut fake = Fake::new();
        fake.sources = vec![(
            "src/caller.rs".to_string(),
            vec![
                "fn caller() {".into(),
                "    watch_ready();".into(),
                "}".into(),
            ],
        )];
        let mut brief = Brief {
            signal: Some(Signal::Strong),
            finished: true,
            answered: 1,
            ..Brief::default()
        };
        brief.defined = vec![SymbolHit {
            start_line: 280,
            end_line: 400,
            ..symbol(DAEMON, "watch_ready")
        }];
        brief.callers = vec![caller("src/caller.rs", "caller", 2)];
        let plan = Plan::from_prompt("who calls `watch_ready`").unwrap();
        let state = Mutex::new(brief);
        strong_reads(&plan, &fake, &state, Instant::now() + SECOND);
        let brief = state.into_inner().unwrap();
        let text = fit(&brief).unwrap().0;
        assert!(
            text.contains(&format!("\nread: {DAEMON}:280-339 — watch_ready\n")),
            "{text}"
        );
        assert!(
            text.contains("\nalso: src/caller.rs:1-3 — caller\n"),
            "{text}"
        );
        assert!(text.ends_with(READ_LINE), "{text}");
        // No resolved definition, nothing to read.
        let none = Mutex::new(Brief {
            signal: Some(Signal::Strong),
            ..Brief::default()
        });
        strong_reads(&plan, &fake, &none, Instant::now() + SECOND);
        assert!(none.into_inner().unwrap().reads.is_empty());
    }

    #[test]
    fn every_brief_that_ends_on_ranges_should_say_to_search_as_usual_if_they_fail() {
        assert_eq!(
            READ_LINE,
            "Read these ranges (in parallel, one turn) before any search. If they don't answer, search as usual."
        );
        let range = || ReadRange {
            path: "a.rs".into(),
            first: 1,
            last: 9,
            symbol: None,
        };
        let mut high = zeroing_brief(&[DAEMON], 1);
        high.reads = vec![range()];
        let mut strong = Brief {
            signal: Some(Signal::Strong),
            finished: true,
            answered: 1,
            reads: vec![range()],
            ..Brief::default()
        };
        strong.defined = vec![symbol("a.rs", "f")];
        let mut low = low_brief(vec![rhit("a.rs", 1, "x")]);
        low.finished = true;
        low.reads = vec![range()];
        for (name, brief) in [("high", high), ("strong", strong), ("low", low)] {
            let (text, _) = fit(&brief).unwrap();
            assert!(text.ends_with(READ_LINE), "{name}: {text}");
            assert!(
                text.contains("read: a.rs:1-9") || text.contains("read: "),
                "{name}"
            );
        }
    }

    #[test]
    fn a_code_shaped_prompt_with_one_short_definition_can_show_it_as_the_answer() {
        let mut fake = Fake::new();
        let body: Vec<String> = (0..5).map(|n| format!("    step_{n}();")).collect();
        fake.sources = vec![("src/f.rs".to_string(), {
            let mut lines = vec!["fn before() {}".to_string()];
            lines.push("pub fn f() {".into());
            lines.extend(body);
            lines.push("}".into());
            lines
        })];
        let mut brief = Brief {
            signal: Some(Signal::Strong),
            finished: true,
            answered: 1,
            unambiguous_def: true,
            ..Brief::default()
        };
        brief.defined = vec![SymbolHit {
            start_line: 2,
            end_line: 8,
            ..symbol("src/f.rs", "f")
        }];
        let state = Mutex::new(brief.clone());
        strong_answer(&fake, &state, &brief, Instant::now() + SECOND);
        let shown = state.into_inner().unwrap();
        assert!(shown.zero());
        let text = fit(&shown).unwrap().0;
        assert!(text.contains("\nanswer src/f.rs:2:\n"), "{text}");
        assert!(text.contains("  2| pub fn f() {"), "{text}");
        assert!(text.ends_with(ZERO_LINE), "{text}");
        assert!(!text.contains("read: "), "{text}");
        // Ambiguous, too long, or unreadable: nothing is shown.
        let mut ambiguous = brief.clone();
        ambiguous.unambiguous_def = false;
        let mut long = brief.clone();
        long.defined[0].end_line = 80;
        for case in [ambiguous, long] {
            let state = Mutex::new(case.clone());
            strong_answer(&fake, &state, &case, Instant::now() + SECOND);
            assert!(!state.into_inner().unwrap().zero());
        }
    }

    // ---- the region pack -------------------------------------------------

    const PACK_PROMPT: &str = "What does handleError do and who uses it?";

    fn pack_source() -> Fake {
        let mut fake = Fake::new();
        fake.rows = Some(Ok(rfound(vec![
            rhit(
                "src/ui.ts",
                1,
                "export function handleError(e: Error): string {",
            ),
            rhit("src/page.ts", 3, "  return handleError(e as Error);"),
            rhit("docs/errors.md", 2, "handleError is documented here"),
        ])));
        fake.files = Ok(found(vec![hit("src/ui.ts", 1), hit("src/page.ts", 3)]));
        fake.symbols = Ok(vec![SymbolHit {
            uid: "src/ui.ts#handleError#function".into(),
            name: "handleError".into(),
            kind: "function".into(),
            path: "src/ui.ts".into(),
            start_line: 1,
            end_line: 3,
        }]);
        fake.sources = vec![
            (
                "src/ui.ts".into(),
                vec![
                    "export function handleError(e: Error): string {".into(),
                    "  return \"boom \" + e.message;".into(),
                    "}".into(),
                ],
            ),
            (
                "src/page.ts".into(),
                vec![
                    "export function Page() {".into(),
                    "  try { return 1; } catch (e) {".into(),
                    "  return handleError(e as Error);".into(),
                    "  }".into(),
                    "}".into(),
                ],
            ),
        ];
        fake
    }

    fn packed(fake: Fake, parts: usize) -> Finished {
        try_start_with(
            PACK_PROMPT,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
            model_high,
            Around {
                pack: region_pack::Limit::of(parts),
                ..Around::default()
            },
        )
        .expect("the prompt starts a brief")
        .finish_with_record()
    }

    #[test]
    fn a_pack_should_replace_the_packet_of_a_prompt_that_names_a_symbol() {
        let finished = packed(pack_source(), 1);
        let text = finished.text.expect("a brief");
        assert!(text.starts_with("[PIXEL:BRIEF]\nregion pack — searched handleError in the symbol graph and text index"), "{text}");
        assert!(text.contains("### src/ui.ts:1-3 — handleError definition (function)\n1| export function handleError(e: Error): string {"), "{text}");
        assert!(
            text.contains("### src/page.ts:1-5 — handleError usage in Page\n"),
            "{text}"
        );
        assert!(
            text.contains("3|   return handleError(e as Error);"),
            "{text}"
        );
        assert!(text.ends_with(region_pack::DIRECTIVE), "{text}");
        for gone in [
            "\nfiles:",
            "\nread:",
            "\ncoverage:",
            "\nkind:",
            "docs/errors.md",
        ] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
        let record = finished.record;
        assert_eq!(record.bytes, text.len());
        assert_eq!(
            (
                record.pack_bytes,
                record.pack_parts,
                record.pack_targets,
                record.fallback
            ),
            (text.len(), 1, 1, false)
        );
    }

    #[test]
    fn the_kill_switch_should_leave_the_packet_as_it_was() {
        let finished = packed(pack_source(), 0);
        let text = finished.text.expect("a brief");
        assert!(text.contains("\nfiles: "), "{text}");
        assert!(
            text.contains("\nread: src/ui.ts:1-3 — handleError"),
            "{text}"
        );
        assert!(!text.contains("region pack"), "{text}");
        let record = finished.record;
        assert_eq!(
            (
                record.pack_bytes,
                record.pack_parts,
                record.pack_targets,
                record.fallback
            ),
            (0, 0, 0, false)
        );
    }

    #[test]
    fn a_stale_graph_should_fall_back_to_the_text_search_for_the_definition() {
        let mut fake = pack_source();
        fake.symbols = Err("the graph is stale".into());
        let finished = packed(fake, 1);
        let text = finished.text.expect("a brief");
        assert!(
            text.contains("text index (graph stale: text fallback)"),
            "{text}"
        );
        assert!(
            text.contains("### src/ui.ts:1-3 — handleError definition (function)"),
            "{text}"
        );
        assert!(text.ends_with(region_pack::DIRECTIVE), "{text}");
        assert!(!text.contains("find-symbol"), "{text}");
        assert!(!text.contains("packet partial"), "{text}");
        assert!(finished.record.fallback);
    }

    #[test]
    fn a_pack_with_nothing_to_show_should_leave_the_packet_in_place() {
        let mut fake = pack_source();
        fake.symbols = Ok(Vec::new());
        fake.rows = Some(Ok(rfound(vec![rhit(
            "docs/errors.md",
            2,
            "handleError is documented here",
        )])));
        fake.files = Ok(found(vec![hit("docs/errors.md", 2)]));
        let finished = packed(fake, 1);
        let text = finished.text.expect("a brief");
        assert!(text.contains("\nfiles: docs/errors.md:2"), "{text}");
        assert!(!text.contains("region pack"), "{text}");
        assert_eq!(finished.record.pack_bytes, 0);
    }

    #[test]
    fn a_pack_should_not_spend_the_operation_budget() {
        let with = packed(pack_source(), 1).record;
        let without = packed(pack_source(), 0).record;
        assert_eq!((with.ops, with.answered), (without.ops, without.answered));
    }

    #[test]
    fn the_pack_stage_should_publish_into_the_brief_and_do_nothing_when_off_or_late() {
        let plan = Plan::from_prompt(PACK_PROMPT).unwrap();
        let brief = |parts| Brief {
            signal: Some(Signal::Strong),
            pack_limit: region_pack::Limit::of(parts),
            answered: 1,
            searched: true,
            ..Brief::default()
        };
        let repo = pack_source();
        let on = Mutex::new(brief(1));
        pack_evidence(&plan, &repo, &on, Instant::now() + SECOND);
        let shown = on.into_inner().unwrap();
        assert!(shown.pack.is_some());
        let text = fit(&shown).unwrap().0;
        assert!(text.starts_with("[PIXEL:BRIEF]\nregion pack"), "{text}");
        let off = Mutex::new(brief(0));
        pack_evidence(&plan, &repo, &off, Instant::now() + SECOND);
        assert!(off.into_inner().unwrap().pack.is_none());
        let late = Mutex::new(brief(1));
        let past = Instant::now().checked_sub(SECOND).unwrap();
        pack_evidence(&plan, &repo, &late, past);
        assert!(late.into_inner().unwrap().pack.is_none());
    }

    // ---- the kinds that name no symbol ------------------------------------------

    const TOUR: &str = "I'm new to this repo. give me a quick tour of how it's organised and what each crate is for";
    const REVIEW: &str = "review the latest commit and tell me if anything looks risky";
    const WALK: &str = "walk me through what happens when someone drops an image onto the canvas";

    /// A gate reading of `shared` words in a file of `structural` symbols,
    /// in the off tier.
    fn off_with(shared: usize, structural: usize) -> Gathered {
        let mut got = gathered(Some(Ok(off_topic_answer())), None, Tier::Off);
        if let Some(scored) = got.scored.as_mut() {
            scored.features.shared = shared;
            scored.features.structural_files = structural;
        }
        got
    }

    #[test]
    fn refusal_should_admit_a_tour_and_a_review_but_a_walkthrough_only_beside_the_repositorys_words()
     {
        for signal in [Signal::Prose, Signal::Weak] {
            for kind in [QuestionKind::Overview, QuestionKind::Review] {
                assert_eq!(
                    refusal(signal, &off_with(0, 0), true, true, kind),
                    None,
                    "{signal:?} {kind:?}"
                );
            }
            assert!(
                refusal(signal, &off_with(0, 0), true, true, QuestionKind::Flow).is_some(),
                "{signal:?}: no word shared, no walkthrough"
            );
            assert!(
                refusal(signal, &off_with(5, 0), true, true, QuestionKind::Flow).is_some(),
                "{signal:?}: words shared with files that hold no symbol"
            );
            assert_eq!(
                refusal(signal, &off_with(2, 1), true, true, QuestionKind::Flow),
                None,
                "{signal:?}"
            );
            // Asked about in so many words, history needs the same footing.
            assert_eq!(
                refusal(signal, &off_with(2, 1), true, true, QuestionKind::History),
                None,
                "{signal:?}"
            );
            assert!(
                refusal(signal, &off_with(0, 4), true, true, QuestionKind::History).is_some(),
                "{signal:?}"
            );
            assert!(
                refusal(signal, &off_with(1, 9), true, true, QuestionKind::Lookup).is_some(),
                "{signal:?}: one shared word is not coverage for a kind with no bypass"
            );
        }
        // A tour does not need the probe to have answered at all.
        let failed = gathered(Some(Err("cold".into())), None, Tier::Off);
        assert_eq!(
            refusal(Signal::Prose, &failed, true, true, QuestionKind::Overview),
            None
        );
        assert!(refusal(Signal::Prose, &failed, true, true, QuestionKind::Lookup).is_some());
    }

    /// `off_with`, with the informative-word count the coverage floor weighs.
    fn off_covered(shared: usize, informative: usize, structural: usize) -> Gathered {
        let mut got = off_with(shared, structural);
        if let Some(scored) = got.scored.as_mut() {
            scored.features.informative = informative;
        }
        got
    }

    #[test]
    fn refusal_should_let_an_off_score_through_when_one_file_covers_the_words() {
        for signal in [Signal::Prose, Signal::Weak] {
            for kind in [
                QuestionKind::Lookup,
                QuestionKind::Config,
                QuestionKind::Tests,
            ] {
                // Half or more of the informative words in one file the graph
                // knows by symbol: kinds with no bypass of their own pass.
                assert_eq!(
                    refusal(signal, &off_covered(3, 5, 2), true, true, kind),
                    None,
                    "{signal:?} {kind:?}"
                );
                // The boundary is inclusive: exactly half the words covers.
                assert_eq!(
                    refusal(signal, &off_covered(2, 4, 4), true, true, kind),
                    None,
                    "{signal:?} {kind:?}"
                );
                // The same coverage with no structural footing stays closed.
                assert!(
                    refusal(signal, &off_covered(3, 5, 0), true, true, kind).is_some(),
                    "{signal:?} {kind:?}: shared text is not a named thing"
                );
                // One shared word, however structural the files, is not coverage.
                assert!(
                    refusal(signal, &off_covered(1, 4, 9), true, true, kind).is_some(),
                    "{signal:?} {kind:?}: one shared word is not coverage"
                );
            }
        }
    }

    #[test]
    fn the_plan_should_read_a_tour_a_review_and_a_walkthrough_from_the_words_before_a_verdict() {
        for (prompt, kind) in [
            (TOUR, QuestionKind::Overview),
            (REVIEW, QuestionKind::Review),
            (WALK, QuestionKind::Flow),
        ] {
            for label in [None, Some("bugfix"), Some("feature"), Some("refactor")] {
                let plan = Plan::from_typed(prompt, false, label);
                assert_eq!(plan.kind, kind, "{prompt} {label:?}");
            }
        }
        // Other kinds still follow a verdict.
        let plan = Plan::from_typed("make `retry_loop` faster", false, Some("feature"));
        assert_eq!(plan.kind, QuestionKind::Feature);
    }

    #[test]
    fn admission_should_name_a_kind_open_with_its_reason() {
        let admission = Admission::Kind(QuestionKind::Overview);
        assert_eq!(
            (admission.label(), admission.reason()),
            ("open", Some("kind: overview"))
        );
        assert_eq!(
            Admission::Kind(QuestionKind::Review).reason(),
            Some("kind: review")
        );
        assert_eq!(
            Admission::Kind(QuestionKind::Flow).reason(),
            Some("kind: flow")
        );
        assert_eq!(Admission::Kind(QuestionKind::Lookup).reason(), Some("kind"));
    }

    /// A fake repository that has a map, a README and nothing else.
    struct Mapped(Fake);

    impl Evidence for Mapped {
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
            q: &str,
            limit: usize,
            deadline: Instant,
        ) -> Result<Vec<MeaningHit>, String> {
            self.0.meaning(q, limit, deadline)
        }
        fn repo_map(&self, _: Instant) -> Result<RepoMap, String> {
            Ok(RepoMap {
                files: vec![kinds_file("src/lib.rs")],
                symbols: 1,
                tops: Vec::new(),
                mains: Vec::new(),
            })
        }
        fn lines_at(
            &self,
            path: &str,
            start: u64,
            end: u64,
            deadline: Instant,
        ) -> Result<Vec<String>, String> {
            self.0.lines_at(path, start, end, deadline)
        }
    }

    fn kinds_file(path: &str) -> kinds::MapFile {
        kinds::MapFile {
            path: path.into(),
            lang: "rust".into(),
            symbols: 1,
        }
    }

    fn around_pack() -> Around {
        Around {
            pack: region_pack::Limit::of(3),
            ..Around::default()
        }
    }

    #[test]
    fn a_tour_should_brief_through_a_gate_that_reads_it_as_off_topic() {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.sources = vec![(
            "README.md".to_string(),
            vec!["# Pixel".into(), "A tool.".into()],
        )];
        let finished = try_start_with(
            TOUR,
            OPEN,
            SECOND,
            move |_| Box::new(Mapped(fake)),
            no_verdict,
            model_off,
            around_pack(),
        )
        .unwrap()
        .finish_with_record();
        let text = finished.text.expect("the tour gets a brief");
        assert!(
            text.contains("### repo map\n1 indexed files, 1 symbols (rust 1)"),
            "{text}"
        );
        assert!(
            text.contains("### README.md:1-2 — readme\n1| # Pixel"),
            "{text}"
        );
        assert!(text.ends_with(region_pack::DIRECTIVE), "{text}");
        let record = finished.record;
        assert_eq!(
            (
                record.gate,
                record.reason.as_deref(),
                record.tier,
                record.kind
            ),
            (
                "open",
                Some("kind: overview"),
                Some("off"),
                Some("overview")
            )
        );
        assert!(record.pack_located >= 2, "{}", record.pack_located);
        // The same gate still closes a prompt of any other kind.
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        let closed = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(Mapped(fake)),
            no_verdict,
            model_off,
            around_pack(),
        )
        .unwrap()
        .finish_with_record();
        assert!(closed.text.is_none());
        assert_eq!(closed.record.gate, "closed");
    }

    #[test]
    fn a_tour_without_a_pack_or_without_anything_to_read_should_render_nothing() {
        let fake = || {
            let mut fake = Fake::new();
            fake.relevance = Ok(off_topic_answer());
            fake
        };
        // The pack switched off: the packet has nothing of a tour.
        let off = try_start_with(
            TOUR,
            OPEN,
            SECOND,
            move |_| Box::new(Mapped(fake())),
            no_verdict,
            model_off,
            Around::default(),
        )
        .unwrap()
        .finish_with_record();
        assert!(off.text.is_none());
        // Nothing to read: the fake has no map, no document, no manifest.
        let nothing = try_start_with(
            TOUR,
            OPEN,
            SECOND,
            move |_| Box::new(fake()),
            no_verdict,
            model_off,
            around_pack(),
        )
        .unwrap()
        .finish_with_record();
        assert!(nothing.text.is_none());
    }

    #[test]
    fn a_brief_admitted_for_its_kind_should_render_only_its_pack() {
        let brief = Brief {
            signal: Some(Signal::Prose),
            admission: Admission::Kind(QuestionKind::Review),
            answered: 1,
            files: vec![RichHit {
                path: "src/a.rs".into(),
                line: 1,
                text: None,
            }],
            ..Brief::default()
        };
        assert!(
            fit(&brief).is_none(),
            "the packet of a kind admission says nothing"
        );
    }

    #[test]
    fn probes_get_the_rest_of_the_window_a_floor_when_the_chain_used_it_up_and_never_past_the_ceiling()
     {
        let started = Instant::now();
        let at = |ms: u64| started + Duration::from_millis(ms);
        // Plenty of window left: the end of the window.
        assert_eq!(
            probe_deadline(started, at(3_000), at(500), PROBE_FLOOR),
            at(3_000)
        );
        // The chain used it up at 2.9 s: the floor is added from now.
        assert_eq!(
            probe_deadline(started, at(3_000), at(2_900), PROBE_FLOOR),
            at(4_100)
        );
        // Late, and the floor would cross the ceiling: the ceiling wins.
        assert_eq!(
            probe_deadline(started, at(3_000), at(4_500), PROBE_FLOOR),
            at(5_000)
        );
        assert_eq!(
            probe_deadline(started, at(4_500), at(100), PROBE_FLOOR),
            at(4_500)
        );
        // The `brief` command has no floor: the window is all there is, even when it is over.
        assert_eq!(
            probe_deadline(started, at(750), at(700), Duration::ZERO),
            at(750)
        );
        assert_eq!(
            probe_deadline(started, at(750), at(900), Duration::ZERO),
            at(900)
        );
    }

    #[test]
    fn the_brief_command_never_runs_past_pis_cutoff_whatever_the_hook_setting() {
        assert_eq!(window_of(3_000, false), Duration::from_millis(3_000));
        assert_eq!(
            window_of(3_000, true),
            Duration::from_millis(pixel_setup::config::BRIEF_WINDOW_COMMAND_MAX_MS)
        );
        assert_eq!(window_of(400, true), Duration::from_millis(400));
    }

    #[test]
    fn a_kind_admission_whose_window_ended_before_its_pack_should_render_its_best_files_as_a_lead()
    {
        let ended = Brief {
            signal: Some(Signal::Prose),
            admission: Admission::Kind(QuestionKind::History),
            answered: 1,
            cut: true,
            files: vec![RichHit {
                path: "src/a.rs".into(),
                line: 7,
                text: None,
            }],
            ..Brief::default()
        };
        let (text, stats) = fit(&ended).expect("the window ended: a lead, not silence");
        assert!(text.starts_with(BRIEF_TAG), "{text}");
        assert!(text.contains("confidence: low"), "{text}");
        assert!(text.contains("files: src/a.rs:7"), "{text}");
        assert_eq!(stats, AnswerStats::default());
        let no_files = Brief {
            files: Vec::new(),
            ..ended.clone()
        };
        assert!(fit(&no_files).is_none(), "nothing arrived, nothing to say");
        let in_time = Brief {
            cut: false,
            ..ended
        };
        assert!(
            fit(&in_time).is_none(),
            "a finished brief without its pack stays silent"
        );
    }

    /// The session's files, as a hook with a transcript would collect them.
    fn around_session(recent: &[&str], dirty: &[&str]) -> Around {
        let paths = SessionPaths {
            recent: recent.iter().map(ToString::to_string).collect(),
            dirty: dirty.iter().map(ToString::to_string).collect(),
        };
        Around {
            context: Some(Box::new(move |_| paths)),
            ..Around::default()
        }
    }

    /// Start `prompt` on `fake`, with `around` and a model that puts every
    /// prompt it scores off topic.
    fn briefed_around(prompt: &str, fake: Fake, around: Around) -> Finished {
        try_start_with(
            prompt,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
            model_off,
            around,
        )
        .expect("the prompt starts a brief")
        .finish_with_record()
    }

    /// Start `prompt` on an off-topic repository, with `around`.
    fn briefed_off_topic(prompt: &str, around: Around) -> Finished {
        let mut fake = Fake::new();
        fake.relevance = Ok(off_topic_answer());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "an unrelated lead")]);
        briefed_around(prompt, fake, around)
    }

    /// A short follow-up that names nothing: plain language, so the relevance
    /// gate alone decides it, and few enough words to be lifted.
    const FOLLOW_UP: &str = "now make the daemon faster";

    /// A repository whose relevance probe fails, as it does behind a stale
    /// base shard, while the meaning search still answers.
    fn probe_failed() -> Fake {
        let mut fake = Fake::new();
        fake.relevance = Err("facts: index_stale".into());
        fake.meaning = Ok(vec![lead(DAEMON, 1, "an unrelated lead")]);
        fake
    }

    #[test]
    fn a_short_follow_up_the_gate_would_close_should_be_let_through_on_the_session_files() {
        let finished = briefed_off_topic(
            "now fix the test",
            around_session(&["src/a.rs"], &["src/b.rs", "src/a.rs"]),
        );
        let text = finished.text.expect("the session's files make a brief");
        assert!(text.contains("\nfiles: src/a.rs; src/b.rs\n"), "{text}");
        assert!(text.contains(relevance::LOW_CONFIDENCE_LINE), "{text}");
        assert!(
            !text.contains(DAEMON),
            "the words' own leads were off topic: {text}"
        );
        let record = finished.record;
        assert_eq!((record.gate, record.tier), ("open", Some("low")));
        assert_eq!(
            record.reason.as_deref(),
            Some("context: a short follow-up on the session's files")
        );
        assert_eq!(finished.paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn a_short_follow_up_should_be_let_through_on_the_session_files_when_the_relevance_probe_fails()
    {
        assert_eq!(
            brief_signal(FOLLOW_UP),
            Some((FOLLOW_UP.to_string(), Signal::Prose))
        );
        assert!(is_follow_up(FOLLOW_UP));
        let finished = briefed_around(
            FOLLOW_UP,
            probe_failed(),
            around_session(&["src/a.rs"], &["src/b.rs", "src/a.rs"]),
        );
        let text = finished.text.expect("the session's files make a brief");
        assert!(text.contains("\nfiles: src/a.rs; src/b.rs\n"), "{text}");
        assert!(text.contains(relevance::LOW_CONFIDENCE_LINE), "{text}");
        assert!(
            !text.contains(DAEMON),
            "the words' own leads are left out of a lifted brief: {text}"
        );
        let record = finished.record;
        assert_eq!(
            (record.signal, record.gate, record.tier),
            (Some("prose"), "open", Some("low"))
        );
        assert_eq!(
            record.reason.as_deref(),
            Some("context: a short follow-up on the session's files")
        );
        assert_eq!(finished.paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn a_lifted_follow_up_should_be_briefed_on_the_session_files_when_no_probe_answered() {
        // The relevance probe fails and the meaning search is unavailable: the
        // only evidence left is the session's own files.
        let mut fake = Fake::new();
        fake.relevance = Err("facts: index_stale".into());
        let finished = briefed_around(
            FOLLOW_UP,
            fake,
            around_session(&["src/a.rs"], &["src/b.rs", "src/a.rs"]),
        );
        let text = finished.text.expect("the session's files make a brief");
        assert!(text.contains("\nfiles: src/a.rs; src/b.rs\n"), "{text}");
        assert!(text.contains(relevance::LOW_CONFIDENCE_LINE), "{text}");
        let record = finished.record;
        assert_eq!((record.gate, record.tier), ("open", Some("low")));
        assert_eq!((record.ops, record.answered), (2, 1));
        assert_eq!(finished.paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn the_same_follow_up_without_session_files_should_stay_closed() {
        for around in [Around::default(), around_session(&[], &[])] {
            let finished = briefed_off_topic("now fix the test", around);
            assert_eq!(finished.text, None);
            assert_eq!(finished.record.gate, "closed");
        }
        // A failed probe closes it as well when there is no session to lift it.
        for around in [Around::default(), around_session(&[], &[])] {
            let finished = briefed_around(FOLLOW_UP, probe_failed(), around);
            assert_eq!(finished.text, None);
            assert_eq!(finished.record.gate, "closed");
            assert_eq!(
                finished.record.reason.as_deref(),
                Some("relevance: facts: index_stale")
            );
        }
    }

    #[test]
    fn a_prompt_that_is_not_a_short_follow_up_should_not_be_lifted_by_the_session_files() {
        let around = || around_session(&["src/a.rs"], &["src/b.rs"]);
        for prompt in [
            "what's the weather going to be like tomorrow",
            "quel temps fera-t-il demain dans la region parisienne",
            "now explain how the quarterly revenue forecast from the finance team changes next year",
        ] {
            let finished = briefed_off_topic(prompt, around());
            assert_eq!(
                (finished.text, finished.record.gate),
                (None, "closed"),
                "{prompt}"
            );
        }
    }

    #[test]
    fn is_follow_up_should_need_few_words_and_a_change_or_a_lean_on_what_came_before() {
        for yes in [
            "now fix the test",
            "also update the docs",
            "do the same for the other one",
            "rename it",
            "maintenant corrige le test",
            "Fix that, please!",
        ] {
            assert!(is_follow_up(yes), "{yes}");
        }
        for no in [
            "",
            "   ",
            "what's the weather going to be like tomorrow",
            "how does the daemon decide when to rebuild the graph after a commit lands in the middle of startup",
        ] {
            assert!(!is_follow_up(no), "{no:?}");
        }
        assert_eq!(FOLLOW_UP_WORDS, 10);
    }

    #[test]
    fn the_session_files_should_lift_a_lead_they_name_and_never_add_a_file_of_their_own() {
        let leads = [
            lead("x.rs", 1, "x"),
            lead("y.rs", 1, "y"),
            lead("z.rs", 1, "z"),
        ];
        let plain: Vec<String> = fuse_with(&leads, &[], &[], false)
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        assert_eq!(plain, vec!["x.rs", "y.rs", "z.rs"]);
        let session = ["z.rs".to_string(), "dirty-and-unrelated.rs".to_string()];
        let boosted: Vec<String> = fuse_with(&leads, &[], &session, false)
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        assert_eq!(boosted, vec!["z.rs", "x.rs", "y.rs"]);
        // The session stands alone only for a lifted follow-up, newest first.
        let only: Vec<(String, u64)> = fuse_with(&leads, &[], &session, true)
            .into_iter()
            .map(|hit| (hit.path, hit.line))
            .collect();
        assert_eq!(
            only,
            vec![
                ("z.rs".to_string(), 0),
                ("dirty-and-unrelated.rs".to_string(), 0)
            ]
        );
        let credential = ["config/.env".to_string()];
        assert!(fuse_with(&[], &[], &credential, true).is_empty());
    }

    #[test]
    fn the_session_files_should_boost_an_on_topic_prompt_without_changing_its_gate() {
        let mut fake = Fake::new();
        fake.relevance = Ok(on_topic_answer());
        fake.meaning = Ok(vec![
            lead(DAEMON, 280, "fn watch_ready() {"),
            lead("crates/pixel-daemon/src/api.rs", 40, "fn op_status"),
            lead("crates/pixel-daemon/src/lib.rs", 3, "mod api;"),
        ]);
        let finished = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(fake),
            no_verdict,
            model_high,
            around_session(&["crates/pixel-daemon/src/lib.rs"], &[]),
        )
        .unwrap()
        .finish_with_record();
        assert_eq!(finished.record.gate, "open");
        let text = finished.text.expect("an on-topic prompt is briefed");
        let files = text
            .lines()
            .find(|line| line.starts_with("files:"))
            .unwrap();
        let lib = files.find("crates/pixel-daemon/src/lib.rs").unwrap();
        let api = files.find("crates/pixel-daemon/src/api.rs").unwrap();
        assert!(
            lib < api,
            "the file the session just read ranks above a lead it did not: {files}"
        );
    }

    #[test]
    fn a_judged_prompt_should_be_mirrored_to_the_central_store_with_the_root() {
        let _g = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let root = scratch_dir("central-mirror");
        // SAFETY: under ENV_LOCK, single-threaded test setup.
        unsafe { std::env::set_var(decision_log::LOG_ENV, "1") };
        // The central path derives from HOME, so point it at the scratch dir
        // before the brief runs, or the mirror lands in the real home.
        let home = std::env::var("HOME").unwrap_or_default();
        // SAFETY: under ENV_LOCK, single-threaded test setup.
        unsafe { std::env::set_var("HOME", root.as_os_str()) };
        let finished = try_start_with(
            PROSE,
            OPEN,
            SECOND,
            move |_| Box::new(Fake::new()),
            no_verdict,
            model_high,
            Around::logging_with_central(
                root.join(".pixel/brief-decisions.jsonl"),
                Some(root.clone()),
            ),
        )
        .unwrap()
        .finish_with_record();
        // The path derives from HOME, so take it before HOME is restored.
        let path = decision_log::central_log_path(&root).unwrap();
        // SAFETY: under ENV_LOCK, single-threaded test setup.
        unsafe { std::env::set_var("HOME", home.as_str()) };
        // SAFETY: under ENV_LOCK, single-threaded test setup.
        unsafe { std::env::remove_var(decision_log::LOG_ENV) };
        let text = std::fs::read_to_string(&path).expect("central log written");
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines.is_empty(), "at least one central record");
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        let canon = root.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(last["root"], serde_json::json!(canon));
        assert_eq!(last["gate"], finished.record.gate);
        assert_eq!(last["typed"], finished.record.typed);
    }

    /// A pack for `PACK_PROMPT` in session `s` of `root`, with what the
    /// session was sent already.
    fn packed_in(root: &Path, seen: Seen) -> Finished {
        try_start_with(
            PACK_PROMPT,
            OPEN,
            SECOND,
            move |_| Box::new(pack_source()),
            no_verdict,
            model_high,
            Around {
                pack: region_pack::Limit::of(1),
                seen,
                sent: Some((root.to_path_buf(), "s".to_string())),
                ..Around::default()
            },
        )
        .expect("the prompt starts a brief")
        .finish_with_record()
    }

    #[test]
    fn a_pack_should_be_remembered_for_the_session_and_not_sent_to_it_again() {
        let root = scratch_dir("sent-once");
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        let now = || pixel_task::now_ms();
        let first = packed_in(&root, Seen::default());
        assert!(first.record.pack_bytes > 0, "{:?}", first.text);
        let seen = sent::load(&root, "s", now());
        assert!(seen.overlaps("src/ui.ts", 1, 3), "the definition was sent");
        assert!(seen.overlaps("src/page.ts", 3, 3), "the usage was sent");
        // The next prompt of the session finds the same code and sends none of it.
        let second = packed_in(&root, seen);
        assert_eq!(second.record.pack_bytes, 0, "{:?}", second.text);
        assert!(
            !second.text.unwrap_or_default().contains("\n### "),
            "no block of source is repeated"
        );
        // A session the host started over is sent the pack again.
        sent::reset(&root, "s");
        let third = packed_in(&root, sent::load(&root, "s", now()));
        assert_eq!(third.record.pack_bytes, first.record.pack_bytes);
    }

    #[test]
    fn a_brief_with_no_pack_should_remember_nothing() {
        let root = scratch_dir("sent-none");
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        let finished = try_start_with(
            PACK_PROMPT,
            OPEN,
            SECOND,
            move |_| Box::new(pack_source()),
            no_verdict,
            model_high,
            Around {
                pack: region_pack::Limit::of(0),
                sent: Some((root.clone(), "s".to_string())),
                ..Around::default()
            },
        )
        .unwrap()
        .finish_with_record();
        assert!(finished.text.is_some());
        assert!(sent::load(&root, "s", pixel_task::now_ms()).is_empty());
    }

    #[test]
    fn finishing_should_allow_probe_recovery_but_preserve_denials_and_explicit_silence() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let plugin = root.join(".pixel/plugins/recovery");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("pixel-plugin.toml"),
            "name = \"recovery\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"recovery evidence\"\nrun = \"probe\"\ncapabilities = [\"brief-probe\"]\n[brief-probe]\nowner = \"test\"\nbudget_ms = 400\nwhen_prompt = \"crate\"\nwhen_gate_closed = true\n",
        ).unwrap();
        let program = plugin.join("probe");
        std::fs::write(
            &program,
            "#!/bin/sh\ncat >/dev/null\nprintf '### source.rs:1-1 — evidence\\n1| pub struct Library;\\n'\n",
        ).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (admission, silenced, expected) in [
            (Admission::Closed("no graph vocabulary".into()), true, true),
            (Admission::Unjudged, false, true),
            (Admission::Kind(QuestionKind::Overview), false, true),
            (Admission::Denied("not a coding task".into()), true, false),
            (Admission::Denied("not a coding task".into()), false, false),
            (Admission::Open, true, false),
        ] {
            let label = admission.label();
            let (send, done) = mpsc::channel();
            send.send(()).unwrap();
            let started = Instant::now();
            let pending = Pending {
                state: Arc::new(Mutex::new(Brief {
                    admission,
                    silenced,
                    ..Brief::default()
                })),
                done,
                deadline: started + SECOND,
                started,
                log: None,
                central: None,
                kicked: None,
                task: "what does this crate expose".into(),
                signal: Signal::Prose,
                sent: None,
                probes: Some(ProbeSetup {
                    host: pixel_plugin::Host {
                        home: root.clone(),
                        repo_root: None,
                        path: std::ffi::OsString::new(),
                        github_base: None,
                    },
                    root: root.clone(),
                    cap: 1_000,
                    floor: Duration::ZERO,
                    max_bytes: None,
                }),
            };
            let finished = pending.finish_with_record();
            if expected {
                assert_eq!(
                    finished.text.as_deref(),
                    Some("[PIXEL:BRIEF]\n### source.rs:1-1 — evidence\n1| pub struct Library;\n"),
                    "{label}"
                );
                assert_eq!(finished.record.probes.len(), 1, "{label}");
                assert_eq!(finished.record.probes[0].outcome, "ok", "{label}");
                if label == "closed" {
                    assert!(
                        !finished.record.enforced,
                        "recovery bypassed the relevance verdict"
                    );
                }
            } else {
                assert_eq!(finished.text, None, "{label}");
                assert_eq!(finished.record.probes, [], "{label}");
            }
        }
    }
}
