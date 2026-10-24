// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The prompt-start evidence brief: a bounded chain of PIXEL lookups the
//! product runs itself, rendered as one compact `[PIXEL:BRIEF]` block.
//!
//! The fixed prefix is `search-content` on the first anchor, `find-code`
//! when that found no file, `find-symbol` for a uid, and `impact` on that
//! uid when the prompt asks about a change or its callers. The
//! [`QuestionKind`] the prompt routes to then spends the ops that are left
//! on the evidence shape the question asked: the witness hops of a call path
//! for a flow, covering tests, `list-signatures` and JSON admission for a
//! config question, a history search for a rationale, and the definition's
//! bounded body for a bugfix. Everything shares one deadline and at most
//! [`MAX_OPS`] operations — the deadline, not the count, is the invariant —
//! runs on its own thread, and renders whatever it has when the deadline
//! passes, so it never holds the prompt back. The lookups sit behind
//! [`Evidence`]: the daemon is the live source, tests bring their own.
//!
//! A prompt in plain language (no identifier, no code word) starts differently:
//! the relevance probe and the meaning search run at once on the shared
//! deadline, and the relevance decision, with no model in it, says whether the
//! prompt is about this repository at all. Off topic ends the brief at once and
//! renders nothing; on topic fuses the meaning leads with the files the
//! prompt's words meet in, and the routed kind goes on from there. A weakly
//! code-shaped prompt gets the same probes as its retriever; the same decision
//! silences it when [`ENFORCE_GATE_ON_WEAK`] is set, and a code graph that did
//! not answer leaves its pre-gate brief in place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::answer::{self, Excerpt, Hop, Receipt};
use crate::decision::{self, Record, Stages};
use crate::kinds::{self, ChangeScope, Changes, FlowNode, KindInput, RepoMap};
use crate::probes::{self, ProbeSetup};
use crate::prompt::{SOURCE_EXTENSIONS, Signal, brief_signal, names_code};
use crate::region_pack;
use crate::relevance::{self, Basis, GateInput, RelevanceInput, Tier};
use crate::routes::QuestionKind;
use crate::sent::{self, Seen};
use crate::session_paths::{self, SessionPaths};
use crate::steps::{self, Steps};

/// Opening line of the block; a host and a test find the brief by it.
pub const BRIEF_TAG: &str = "[PIXEL:BRIEF]";
/// The window one brief may take when the settings do not say, for the hook
/// that runs it: three seconds, because a machine running a dozen agent
/// sessions slows every lookup at once and a window that ends first leaves
/// the agent no brief at all.
pub const WINDOW_HOOK_DEFAULT_MS: u64 = 3_000;
/// The same for the `brief` command, which OpenCode's extension cuts off.
pub const WINDOW_COMMAND_DEFAULT_MS: u64 = 750;
/// The most a configured window may give the `brief` command.
pub const WINDOW_COMMAND_MAX_MS: u64 = 1_000;
/// The shortest window a setting may ask for.
pub const WINDOW_MIN_MS: u64 = 250;
/// The longest: under the hook's own limit with room for the parts after
/// the first and for printing.
pub const WINDOW_MAX_MS: u64 = 4_500;
const _: () = assert!(WINDOW_MIN_MS <= WINDOW_COMMAND_DEFAULT_MS);
const _: () = assert!(WINDOW_HOOK_DEFAULT_MS <= WINDOW_MAX_MS);

/// The window for a configured `ms` (`None`: the default of the caller),
/// clamped to what the hook that runs the brief can wait for. The `brief`
/// command is cut off by OpenCode's extension after 1.2 s, so a setting tuned
/// for the hook never lets it run past [`WINDOW_COMMAND_MAX_MS`].
pub fn window_for(ms: Option<u64>, command: bool) -> Duration {
    let default = if command {
        WINDOW_COMMAND_DEFAULT_MS
    } else {
        WINDOW_HOOK_DEFAULT_MS
    };
    let ms = ms.unwrap_or(default).clamp(WINDOW_MIN_MS, WINDOW_MAX_MS);
    Duration::from_millis(if command {
        ms.min(WINDOW_COMMAND_MAX_MS)
    } else {
        ms
    })
}

/// Operations one brief may start, whatever they answer. A kind route may
/// use the headroom (search, concept, symbols, impact, context, one
/// kind op); the shared deadline, not this count, is the invariant.
pub const MAX_OPS: usize = 6;
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
pub const SEARCH_ROWS: usize = 200;
/// Matches one concept search keeps.
pub const CONCEPT_ROWS: usize = 8;
/// Symbols one name lookup reads.
pub const SYMBOL_ROWS: u32 = 50;

const MAX_ANCHORS: usize = 4;
const MAX_ANCHOR_CHARS: usize = 80;
/// Shortest snake_case or camelCase word taken for an anchor.
const MIN_CASED_CHARS: usize = 5;
const MAX_FILES: usize = 8;
const MAX_DEFINED: usize = 3;
const MAX_CALLERS: usize = 10;
/// Longest concept phrase: all the significant words the prompt carries,
/// bounded by characters rather than a word count.
const MAX_CONCEPT_CHARS: usize = 200;
const MIN_CONCEPT_WORD_CHARS: usize = 4;
const MAX_ITEM_CHARS: usize = 120;
/// Chars the `defined` line gives the packed body of the picked
/// definition: wide enough for a few source lines, bounded so it cannot
/// own the whole block.
const DEF_BODY_CHARS: usize = 480;
/// Chars of the typed prompt kept for follow-ups.
const MAX_TYPED_CHARS: usize = 600;
/// History rows a rationale question pulls.
const HISTORY_ROWS: usize = 3;
/// Paths the journal's `turn` line may carry from one brief.
const MAX_INJECTED_PATHS: usize = 32;
/// Token budget of a `pack-context` op.
const CONTEXT_BUDGET_TOKENS: usize = 400;

/// Stems of the words that ask about a change or about who depends on a
/// symbol (`callers`, `depends`, `deprecated` all contain their stem).
const CHANGE_STEMS: &[&str] = &[
    "rename", "remove", "delete", "deprecat", "impact", "caller", "depend", "refactor", "affect",
    "break", "blast",
];
/// Words that request edits or usage sites without routing through impact.
const EDIT_STEMS: &[&str] = &[
    "chang",
    "modif",
    "replac",
    "fix",
    "add ",
    "added",
    "adding",
    "move",
    "updat",
    "signature",
    "implement",
    "edit",
    "rewrit",
    "extract",
    "uses",
    "used",
    "usage",
    "who use",
    "call",
    "refer",
    "utilis",
    "appel",
    "renomm",
    "remplac",
    "supprim",
    "ajout",
    "corrig",
    "déplac",
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
pub struct FileHit {
    pub path: String,
    pub line: u64,
}

/// The answer of a file search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub hits: Vec<FileHit>,
    /// The search stopped at its row cap: the list is a prefix.
    pub capped: bool,
}

/// A file hit whose source carried the matched text itself — the line the
/// pattern hit, or the concept's own words — so the `files` line can show
/// why each file is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichHit {
    pub path: String,
    pub line: u64,
    /// The matched text the source carried, when it carried any.
    pub text: Option<String>,
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
pub struct RichFound {
    pub hits: Vec<RichHit>,
    /// The search stopped at its row cap: the list is a prefix.
    pub capped: bool,
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
pub enum Flow {
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
pub struct StatusProbe {
    /// `facts.fresh` the status op reported; `None` when no route could
    /// say (local route, or a daemon without facts visibility).
    pub facts_fresh: Option<bool>,
}

/// One history row: the commit's short sha and its subject line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryHit {
    pub sha: String,
    pub subject: String,
    /// When the commit was made (the index's timestamp), if it said.
    pub at: String,
    /// The path the hit is about (a path or diff hit), if any.
    pub path: Option<String>,
    /// The changed lines the hit matched (a diff hit), if any.
    pub snippet: Option<String>,
}

/// One declaration `find-symbol` reports; `uid` is `path#name#kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolHit {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
}

/// One function on a call path, with where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathHop {
    pub symbol: SymbolHit,
    /// The line, in the previous hop's file, of the call that reaches this
    /// hop; 0 for the entry.
    pub call_line: u64,
    /// Whether the call that reaches this hop was matched by name, not proved.
    pub likely: bool,
}

/// A direct caller `impact` reports: where it lives and which symbol calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerHit {
    pub path: String,
    pub via: String,
    pub line: u64,
}

/// One lead of a natural-language search over code: the best chunk of a file
/// for the question. A lead ranks, it does not prove relevance.
#[derive(Clone, Debug, PartialEq)]
pub struct MeaningHit {
    pub path: String,
    /// Inclusive 1-based line range of the chunk.
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol the chunk belongs to, when it belongs to one.
    pub symbol: Option<String>,
    /// Comparable within one answer, not across questions.
    pub score: f64,
    /// The head of the chunk on one line.
    pub snippet: String,
}

/// The answer of a relevance probe: the weights the gate scores (the
/// daemon's, so the formula has one spelling) and the line each co-file was
/// found at.
#[derive(Clone, Debug)]
pub struct RelevanceAnswer {
    pub input: RelevanceInput,
    /// The co-files in the input's order, each with the line and text that
    /// showed its keywords.
    pub lines: Vec<RichHit>,
}

/// A brief can stop waiting while its worker still runs: the age must be
/// recorded before an answered operation publishes its rows to that brief.
#[derive(Clone, Debug, Default)]
pub struct Staleness(Arc<AtomicBool>);

/// A warning about evidence, not the current text read from cited files.
const STALENESS_LINE: &str =
    "staleness: some daemon evidence predates the last edit; verify against the current files.";

impl Staleness {
    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn line(&self) -> Option<&'static str> {
        self.0.load(Ordering::Acquire).then_some(STALENESS_LINE)
    }
}

/// The lookups of the chain. Every method answers within `deadline` or
/// fails; none of them builds, refreshes or starts anything. `Sync`: a
/// plain-language prompt asks several of them from threads at once.
pub trait Evidence: Sync {
    /// Shared age of successful daemon evidence, including in-flight reads.
    fn staleness(&self) -> Staleness {
        Staleness::default()
    }

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
    /// The shortest chain of resolved calls from the function `from` to the
    /// function `to` (uids), the entry first: each hop with its place and the
    /// line, in the previous hop's file, of the call that reaches it. `None`
    /// when no chain exists.
    fn call_path(
        &self,
        from: &str,
        to: &str,
        _deadline: Instant,
    ) -> Result<Option<Vec<PathHop>>, String> {
        let _ = (from, to);
        Err("call path unsupported".to_string())
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
    /// The text of one read-only git command (`log`, `show`, `rev-list` or
    /// `tag`, named first) in the repository, at most `max_bytes` of it,
    /// within `deadline`. An `Err` is a command that failed, ran out of time,
    /// printed more than `max_bytes` or is not one of those four.
    fn git_read(
        &self,
        args: &[&str],
        max_bytes: usize,
        _deadline: Instant,
    ) -> Result<String, String> {
        let _ = (args, max_bytes);
        Err("git unsupported".to_string())
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
    /// Every file the repository tracks, sorted by path: what a recipe step
    /// finds manifests, READMEs and a member's files in. Not an index
    /// operation, like [`Evidence::line_at`].
    fn tracked_files(&self, _deadline: Instant) -> Result<Vec<String>, String> {
        Err("tracked files unsupported".to_string())
    }
    /// The text of the tracked regular file `path`: refused when it is a
    /// link, not a regular file, credential-shaped, or longer than
    /// `max_bytes`. A plain read outside the op budget.
    fn read_file(&self, path: &str, max_bytes: u64, _deadline: Instant) -> Result<String, String> {
        let _ = (path, max_bytes);
        Err("file read unsupported".to_string())
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
    /// The routed evidence shape, from the typed-text heuristic.
    kind: QuestionKind,
    /// The typed prompt, bounded — the text a `next:` suggestion quotes.
    typed: String,
    concept: Option<String>,
    /// Which recipe steps run.
    steps: Steps,
}

impl Plan {
    fn with_steps(self, steps: Steps) -> Self {
        Self { steps, ..self }
    }

    /// `None` for a prompt that does not ask about code, pasted text aside, and
    /// for a continuation or a harness envelope, which are not the user's task.
    /// Tests plan a chain directly.
    #[cfg(test)]
    pub(crate) fn from_prompt(prompt: &str) -> Option<Self> {
        if crate::continuation::is_trivial_continuation(prompt) {
            return None;
        }
        let (typed, _) = crate::prompt::code_signal(prompt)?;
        Some(Self::from_typed(&typed))
    }

    /// The plan of typed text already judged about-code.
    fn from_typed(typed: &str) -> Self {
        let anchors = Anchors::from_text(typed);
        let kind = QuestionKind::heuristic(typed, &anchors);
        Self {
            anchors,
            // A bugfix prompt wants the symbol's blast radius even when the
            // phrasing held no change stem.
            change_intent: has_change_intent(typed) || kind == QuestionKind::Bugfix,
            kind,
            typed: typed.chars().take(MAX_TYPED_CHARS).collect(),
            concept: concept_phrase(typed),
            steps: Steps::default(),
        }
    }
}

/// The prompt asks about a change or about who depends on something.
pub(crate) fn has_change_intent(typed: &str) -> bool {
    let lower = typed.to_lowercase();
    CHANGE_STEMS.iter().any(|stem| lower.contains(stem))
}

fn is_lookup_only(plan: &Plan) -> bool {
    let lower = plan.typed.to_lowercase();
    !plan.change_intent && !EDIT_STEMS.iter().any(|stem| lower.contains(stem))
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
pub fn is_generated(path: &str) -> bool {
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

/// What the gate decided about the prompt: the relevance probe, for the
/// journal and for the render.
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
    /// The relevance gate would have closed the prompt, but it asks for a
    /// tour, a review or a walkthrough, which the repository's words cannot
    /// say: the kind admits it.
    Kind(QuestionKind),
    /// The relevance gate would have closed a short follow-up ("now fix the
    /// test"), but the session is already working on files: they stand for
    /// what the words cannot say.
    Context,
    /// The relevance gate would have closed the prompt, but a recipe step
    /// answered it from the manifests (`steps`): the repository's words are
    /// not what a question about its workspace is about.
    Steps,
}

impl Admission {
    /// The decision as the log spells it.
    fn label(&self) -> &'static str {
        match self {
            Self::Unjudged => "unjudged",
            Self::Open => "open",
            Self::Closed(_) => "closed",
            Self::Kind(_) | Self::Context | Self::Steps => "open",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Closed(reason) => Some(reason),
            Self::Kind(kind) => Some(match kind {
                QuestionKind::Overview => "kind: overview",
                QuestionKind::Review => "kind: review",
                QuestionKind::Flow => "kind: flow",
                QuestionKind::History => "kind: history",
                _ => "kind",
            }),
            Self::Context => Some("context: a short follow-up on the session's files"),
            Self::Steps => Some("steps: answered from the manifests"),
            Self::Unjudged | Self::Open => None,
        }
    }
}

/// What the chain has learned, shared between the worker and the hook.
#[derive(Clone, Debug, Default)]
pub(crate) struct Brief {
    /// How long each recipe step that ran took, in milliseconds.
    step_ms: Vec<(&'static str, u64)>,
    staleness: Staleness,
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
    /// The `confidence:` line of a brief built on the relevance decision.
    confidence: Option<String>,
    anchors: Vec<String>,
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
    /// files, ranges, excerpts, definitions, and tests the lines list.
    fn injected_paths(&self, packed: bool) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        let mut add = |path: &str| {
            if !path.is_empty()
                && paths.len() < MAX_INJECTED_PATHS
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
            if admit_json && json && pixel_model::credential_path(Path::new(&hit.path)) {
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
        if pixel_model::credential_path(Path::new(path)) || is_generated(path) {
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
    edit(state, |brief| brief.staleness = evidence.staleness());
    let admit_json = plan.kind == QuestionKind::Config;
    edit(state, |brief| {
        brief.anchors.clone_from(&plan.anchors.0);
        brief.kind = Some(plan.kind);
        // The fallback follow-up of a partial packet; each routed kind
        // overwrites it with its own command.
        brief.next = plan
            .concept
            .clone()
            .map(|phrase| format!("pixel find-code {}", crate::routes::q(&phrase)));
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
        if !edit(state, |brief| brief.cut) {
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
    let terms = spelling_terms(brief.receipt.as_ref().map_or(&[][..], Receipt::terms));
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
    let mut by_kind = !located.is_empty();
    let near: Vec<String> = brief
        .files
        .iter()
        .map(|hit| hit.path.clone())
        .chain(brief.defined.iter().map(|hit| hit.path.clone()))
        .chain(brief.reads.iter().map(|read| read.path.clone()))
        .collect();
    let steps::Outcome {
        lead,
        append,
        drops,
        timings,
        ..
    } = steps::blocks_held(
        &plan.typed,
        plan.kind,
        plan.steps,
        &near,
        steps::Held {
            history: !brief.history.is_empty(),
        },
        evidence,
        deadline,
    );
    edit(state, |brief| {
        brief.step_ms = timings
            .iter()
            .map(|run| (run.step.name(), run.ms))
            .collect();
    });
    // The regions closest to a few words are not the repository, nor the
    // change; and the words of a review ("commit", "latest", "tests") are
    // not a topic to look for tests of.
    let own = kinds::owns_the_pack(plan.kind);
    if !own {
        located.extend(crate::locator::locate(
            &plan.typed,
            evidence,
            brief.receipt.as_ref(),
            deadline,
        ));
    }
    by_kind |= !lead.is_empty() || !append.is_empty();
    // What the steps took out of the rest of the pack leaves the located
    // blocks here and the candidates in the pack; the steps' own stay.
    located.retain(|block| !drops.drops(block));
    let exempt: Vec<String> = lead
        .iter()
        .chain(&append)
        .map(|block| block.head().to_string())
        .collect();
    located.splice(0..0, lead);
    located.extend(append);
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
        lookup_only: is_lookup_only(plan),
        locators: located,
        drops,
        exempt,
        passes: crate::passes::Passes {
            rank: plan.steps.enabled(steps::Step::RankBlocks)
                && !matches!(
                    plan.kind,
                    QuestionKind::Overview
                        | QuestionKind::Review
                        | QuestionKind::Flow
                        | QuestionKind::History
                ),
            prune: plan.steps.enabled(steps::Step::PruneNoise),
            prompt: plan.typed.clone(),
            names: plan.anchors.names().map(ToString::to_string).collect(),
        },
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

/// The receipt's terms that can spell a name in the pack: the ones that say
/// what is asked ("cover" in "which tests cover…", "behaviour", "number") are
/// left out, or they anchor the pack on `coverUrlForThumbnail`.
fn spelling_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| !crate::locator::is_generic(&term.to_lowercase()))
        .cloned()
        .collect()
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
                .filter(|hit| !pixel_model::credential_path(Path::new(&hit.path)));
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
        .filter(|hit| !pixel_model::credential_path(Path::new(&hit.path)))
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
        if lines.len() >= answer::MAX_TESTS || pixel_model::credential_path(Path::new(path)) {
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
            && !pixel_model::credential_path(Path::new(&hit.path))
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
    if pixel_model::credential_path(Path::new(&def.path)) {
        return;
    }
    if answer::STRONG_ZERO {
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
        if ranges.len() >= MAX_READS || pixel_model::credential_path(Path::new(&caller.path)) {
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
            || pixel_model::credential_path(Path::new(&candidate.path))
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
                    .map(|hit| format!("pixel impact {}", crate::routes::q(&hit.uid)));
            });
            def_body(evidence, state, deadline, false);
        }
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
            brief.next = Some(format!("pixel who-calls {}", crate::routes::q(&from)));
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
            "pixel call-path {} {}",
            crate::routes::q(&from),
            crate::routes::q(&to)
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
        if crate::routes::asks_flow(&word) {
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
        brief.next = Some(format!("pixel who-calls {}", crate::routes::q(&uid)));
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
        brief.next = Some(format!("pixel list-signatures {}", crate::routes::q(&file)));
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
            "pixel search-history {}",
            crate::routes::q(&phrase)
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

/// A literal lookup: `pack-context` on the picked definition only when it
/// was the single unambiguous hit.
fn lookup_evidence(evidence: &dyn Evidence, state: &Mutex<Brief>, deadline: Instant) {
    def_body(evidence, state, deadline, true);
    edit(state, |brief| {
        brief.next = brief
            .defined
            .first()
            .map(|hit| format!("pixel pack-context {}", crate::routes::q(&hit.uid)))
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
pub enum Declined {
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
    /// The reason as the journal spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unindexed => "unindexed",
            Self::Continuation => "continuation",
            Self::NotAboutCode => "not_about_code",
            Self::NoWorker => "no_worker",
        }
    }
}

/// The model that decided the tier, as the journal names it.
fn gate_model_source() -> &'static str {
    crate::gate_model::SOURCE
}

/// Whole milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    u64::try_from(pixel_metrics::now_ms()).unwrap_or(0)
}

/// The record of a prompt that got no brief.
pub fn declined_record(declined: Declined, prompt: &str) -> Record {
    let typed = crate::prompt::typed_text(prompt);
    let typed = typed.trim();
    Record {
        ts_ms: now_ms(),
        signal: None,
        gate: "declined",
        enforced: false,
        reason: Some(declined.as_str().to_string()),
        score: None,
        best_file: None,
        features: None,
        tier: None,
        model: gate_model_source(),
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
        typed: decision::logged(typed),
        sha256: pixel_metrics::journal::sha256_hex(typed),
        probes: Vec::new(),
        steps: Vec::new(),
    }
}

/// How the files of the session are collected for a prompt, by a deadline.
type ContextSource = Box<dyn FnOnce(Instant) -> SessionPaths + Send>;

/// What a host tells the brief about the session a prompt belongs to.
#[derive(Clone, Debug, Default)]
pub struct Session {
    /// The host's id for the session, when it names one: the key of what the
    /// session has been sent (see `sent`).
    pub id: Option<String>,
    /// The host's transcript of the session, when it names one.
    pub transcript: Option<PathBuf>,
}

/// What a brief does around its chain besides running it.
#[derive(Default)]
pub(crate) struct Around {
    /// What the region pack may be; the default leaves the packet as it was.
    pub(crate) pack: region_pack::Limit,
    /// The files the session is working on, when the host's hook has a session.
    pub(crate) context: Option<ContextSource>,
    /// Ranges an earlier pack sent to this session: the pack leaves them out.
    pub(crate) seen: Seen,
    /// Where the ranges this pack sends are remembered: the repository and
    /// the session.
    pub(crate) sent: Option<(PathBuf, String)>,
    /// Whether a French prompt is searched and gated on the English words
    /// code uses (`french::english`) instead of its own.
    pub(crate) french: bool,
    /// Which recipe steps run.
    pub(crate) steps: Steps,
    /// Where the brief's probes are looked up and run, when they may be.
    pub(crate) probes: Option<ProbeSetup>,
}

/// A brief on its way: the worker is running, the deadline is fixed.
pub struct Pending {
    state: Arc<Mutex<Brief>>,
    done: Receiver<()>,
    deadline: Instant,
    started: Instant,
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
pub struct Finished {
    pub text: Option<String>,
    pub record: Record,
    /// The repository files the block names, best first: what the agent was
    /// handed to read, for the journal's `turn` line. Empty when there is no
    /// block.
    pub paths: Vec<String>,
}

/// Whether the gate's verdict decided if the brief was shown: always for
/// plain language, for a weak prompt only when [`ENFORCE_GATE_ON_WEAK`] says
/// so.
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
    #[cfg(test)]
    pub(crate) fn finish(self) -> Option<String> {
        self.finish_with_record().text
    }

    /// `text` after the brief's probes, and how each ended. A probe that
    /// opted in (`when_gate_closed`) may answer a prompt the gate closed; none
    /// runs for a prompt the gate silenced for another reason.
    fn probe_stage(
        &self,
        brief: &Brief,
        text: Option<String>,
    ) -> (Option<String>, Vec<probes::ProbeRun>) {
        let Some(setup) = &self.probes else {
            return (text, Vec::new());
        };
        let gate_closed = matches!(brief.admission, Admission::Closed(_));
        if brief.silenced && !gate_closed {
            return (text, Vec::new());
        }
        let kind = brief.kind.map(QuestionKind::as_str);
        let stage = probes::Stage {
            host: &setup.host,
            root: &setup.root,
            prompt: &self.task,
            kind,
            gate_closed,
            deadline: probes::probe_deadline(
                self.started,
                self.deadline,
                Instant::now(),
                setup.floor,
            ),
            cap: setup.cap,
            max_bytes: setup.max_bytes,
        };
        match probes::apply(&stage, text.as_deref().unwrap_or("")) {
            (Some(merged), runs) => (Some(merged), runs),
            (None, runs) => (text, runs),
        }
    }

    /// Wait for the worker until the shared deadline, render what it has and
    /// say what was decided.
    pub fn finish_with_record(self) -> Finished {
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
        let record = Record {
            ts_ms: now_ms(),
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
            elapsed_ms: millis(self.started.elapsed()),
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
            typed: decision::logged(&self.task),
            sha256: pixel_metrics::journal::sha256_hex(&self.task),
            probes: probe_runs,
            steps: brief.step_ms.clone(),
        };
        let paths = if text.is_some() {
            brief.injected_paths(stats.pack_bytes > 0)
        } else {
            Vec::new()
        };
        // What the pack just sent is not sent to this session again.
        if let (Some((root, session)), Some(pack), true) =
            (&self.sent, &brief.pack, stats.pack_bytes > 0)
        {
            let _ = sent::record(root, session, &pack.ranges(), now_ms());
        }
        Finished {
            text,
            record,
            paths,
        }
    }
}

/// What one brief is asked for.
pub struct Request<'a> {
    pub prompt: &'a str,
    /// The repository the evidence is about.
    pub root: &'a Path,
    /// The `brief` command (which OpenCode runs and drops above 4 000 bytes)
    /// rather than a prompt hook.
    pub command: bool,
    /// How many hooks deliver the brief (`--part K/N`: `N`, and 1 for a host
    /// with one hook): the region pack may fill that many.
    pub parts: u8,
    /// The configured window in milliseconds; `None`: the caller's default.
    pub window_ms: Option<u64>,
    /// What the host says of the session. With it the files the session is
    /// already working on (its transcript's recent tool calls, the dirty files
    /// of the working tree) boost the leads, and let a short follow-up
    /// through the gate.
    pub session: Option<Session>,
    /// Whether a French prompt is searched on the English words code uses.
    pub french: bool,
    /// Which recipe steps run (`specs/brief.md`, "Ablation").
    pub steps: Steps,
    /// The plugin host whose brief probes may run on this brief; `None`: no
    /// probe runs (`PIXEL_BRIEF_PROBES=off`, or a caller with no host).
    pub probes: Option<pixel_plugin::Host>,
}

/// Start the brief for `request`, or decline: `open` makes the evidence the
/// worker thread asks, within the deadline.
///
/// # Errors
///
/// [`Declined`] says why the prompt gets no brief: a continuation, a prompt
/// that asks nothing about code, or no thread to run the worker on.
pub fn start<F>(request: &Request, open: F) -> Result<Pending, Declined>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
{
    let root = request.root.to_path_buf();
    let session_id = request
        .session
        .as_ref()
        .and_then(|session| session.id.clone());
    // What this session was sent by earlier prompts, and where this prompt's
    // pack is remembered.
    let seen = session_id
        .as_deref()
        .map(|id| sent::load(&root, id, now_ms()))
        .unwrap_or_default();
    let sent_to = session_id.map(|id| (root.clone(), id));
    let context: Option<ContextSource> = request.session.clone().map(|session| -> ContextSource {
        Box::new(move |deadline| {
            session_paths::collect(&root, session.transcript.as_deref(), deadline)
        })
    });
    let pack = region_pack::limit_for(request.command, request.parts);
    let probes = request.probes.clone().map(|host| {
        ProbeSetup::new(
            host,
            request.root.to_path_buf(),
            request.command,
            pack.parts,
            pack.chars,
        )
    });
    try_start_with(
        request.prompt,
        window_for(request.window_ms, request.command),
        open,
        relevance::judge,
        Around {
            pack,
            context,
            seen,
            sent: sent_to,
            french: request.french,
            steps: request.steps,
            probes,
        },
    )
}

/// [`start`] with its window, evidence source and tier model given: the seam
/// the chain's tests start from.
pub(crate) fn try_start_with<F>(
    prompt: &str,
    window: Duration,
    open: F,
    model: Model,
    around: Around,
) -> Result<Pending, Declined>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
{
    let Around {
        pack,
        context,
        seen,
        sent,
        french,
        steps,
        probes,
    } = around;
    if crate::continuation::is_trivial_continuation(prompt) {
        return Err(Declined::Continuation);
    }
    let Some((typed, signal)) = brief_signal(prompt) else {
        return Err(Declined::NotAboutCode);
    };
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
    // The journal keeps the text as typed; the searches, the gate and the plan
    // read a French prompt in the English words code uses.
    let task = typed.clone();
    let typed = if french {
        crate::french::english(&typed).unwrap_or(typed)
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
                model,
                context: &context,
                steps,
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
        task,
        signal,
        sent,
        probes,
    })
}

/// What the worker of one brief is given.
struct Job<'a> {
    typed: &'a str,
    signal: Signal,
    deadline: Instant,
    state: &'a Mutex<Brief>,
    finished: &'a Sender<()>,
    model: Model,
    /// The session's files on their way, and when they stop being waited for.
    context: &'a Option<(Receiver<SessionPaths>, Instant)>,
    /// Which recipe steps run.
    steps: Steps,
}

/// The worker of one brief: plan, ask, run.
fn work<F>(job: &Job, open: F)
where
    F: FnOnce(Instant) -> Box<dyn Evidence>,
{
    let Job {
        typed,
        signal,
        deadline,
        state,
        finished,
        model,
        context,
        steps,
    } = *job;
    if signal == Signal::Strong {
        let plan = Plan::from_typed(typed).with_steps(steps);
        let evidence = open(deadline);
        edit(state, |brief| {
            brief.route = Some(evidence.route());
            brief.staleness = evidence.staleness();
        });
        run(&plan, evidence.as_ref(), state, deadline);
        return;
    }
    let evidence = open(deadline);
    edit(state, |brief| {
        brief.route = Some(evidence.route());
        brief.staleness = evidence.staleness();
    });
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
    // A recipe step may answer what the gate refuses: the question is about
    // the workspace, not about words the code uses. Asked once, and only when
    // the gate would close the prompt.
    let plan = Plan::from_typed(typed).with_steps(steps);
    let rescue = std::cell::OnceCell::new();
    let rescued = || {
        *rescue.get_or_init(|| {
            steps::blocks(
                &plan.typed,
                plan.kind,
                plan.steps,
                &[],
                evidence.as_ref(),
                deadline,
            )
            .rescuing
        })
    };
    let mut settled = |got: &Gathered| match refusal_lifting(
        signal,
        got,
        false,
        ENFORCE_GATE_ON_WEAK,
        kind,
        lift,
    ) {
        Some(_) if rescued() => false,
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
        model,
        state,
        deadline,
        &mut settled,
    );
    edit(state, mark_searched);
    if edit(state, |brief| brief.silenced) {
        return;
    }
    let refusal = refusal_lifting(signal, &got, true, ENFORCE_GATE_ON_WEAK, kind, lift);
    if let Some(admission) = refusal.clone()
        && !rescued()
    {
        close(state, &got, admission);
        return;
    }
    fold_with(state, &plan, &got, &session, lift);
    if refusal.is_some() {
        // The words were refused, so the files they led to are not the
        // answer: the steps' blocks are, as a pack of their own.
        edit(state, |brief| {
            brief.admission = Admission::Steps;
            brief.files.clear();
            brief.leads.clear();
            brief.lexical.clear();
            brief.receipt = None;
            brief.confidence = None;
            brief.finished = true;
        });
        pack_evidence(&plan, evidence.as_ref(), state, deadline);
        return;
    }
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
        (brief.tier() == Some(Tier::High))
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
}

/// One probe's answer on its way back to the worker.
enum Reply {
    Relevance(Result<RelevanceAnswer, String>),
    Meaning(Result<Vec<MeaningHit>, String>),
    Words(Result<Vec<MeaningHit>, String>),
}

/// Ask the relevance probe and the meaning search at once, each on the
/// shared `deadline`; the probes count against the op budget. Stops
/// collecting as soon as `settled` says the brief is decided, so a refusal
/// does not wait for the slowest sibling.
fn gather(
    typed: &str,
    evidence: &dyn Evidence,
    model: Model,
    state: &Mutex<Brief>,
    deadline: Instant,
    settled: &mut dyn FnMut(&Gathered) -> bool,
) -> Gathered {
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
        drop(send);
        while let Ok(reply) =
            receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            match reply {
                Reply::Relevance(answer) => got.relevance = Some(answer),
                Reply::Meaning(leads) => got.meaning = Some(leads),
                Reply::Words(leads) => got.words = Some(leads),
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
/// Plain language must be shown on topic by the repository; a weak prompt is
/// refused by the relevance decision when `enforce_weak`.
fn refusal_lifting(
    signal: Signal,
    got: &Gathered,
    complete: bool,
    enforce_weak: bool,
    kind: QuestionKind,
    lift: bool,
) -> Option<Admission> {
    match signal {
        Signal::Strong => None,
        Signal::Weak => {
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

/// Keywords one file must hold, at least, for the coverage floor to overrule
/// an off-topic score. Two was the walkthrough's bar, and two words a
/// repository uses (`replay` and `fini`, `merge` and `PRs`) are what a
/// status question or a steer of the agent shares with it: on the #1222
/// replay, three words refused 10 `other` prompts against 1 follow-up on the
/// reference window and 8 against 2 on a window held out from it.
const COVERAGE_MIN_SHARED: usize = 3;

/// A prompt the model read as off topic is still about the repository when
/// one structural co-file holds half or more of its informative keywords,
/// and at least [`COVERAGE_MIN_SHARED`] of them: the words name something
/// the codebase knows by name or symbol.
fn admitted_by_coverage(scored: &relevance::Verdict) -> bool {
    scored.features.shared >= COVERAGE_MIN_SHARED
        && scored.features.shared * 2 >= scored.features.informative
        && scored.features.structural_files > 0
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

/// Keep the decision the probes reached: the relevance verdict.
fn note(brief: &mut Brief, got: &Gathered) {
    brief.relevance.clone_from(&got.scored);
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
        brief.zero_rule = answer::ZERO_RULE;
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
    ranked.retain(|(hit, _)| !pixel_model::credential_path(Path::new(&hit.path)));
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
            excluded: brief.excluded.len(),
            caps: brief.caps.len(),
            unresolved: brief.unresolved.len(),
            excerpt: brief.excerpt_counts(),
        }
    }

    /// Drop one excerpt line, else one entry from the longest list (the earlier of equals in the
    /// order excluded, files, tests, skeleton, history, callers,
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
        let text = match brief.staleness.line() {
            Some(line) => match pack.text_with_notice(line, brief.pack_limit) {
                Some(text) => text,
                None => return render_low(brief).map(|text| (text, AnswerStats::default())),
            },
            None => pack.text_hedged(brief.cut || low),
        };
        let stats = AnswerStats {
            answer: true,
            excerpt_bytes: text.len(),
            pack_bytes: text.len(),
            pack_parts: crate::parts::split(
                &text,
                crate::parts::Limits {
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
        if let Some(line) = brief.staleness.line() {
            text.push('\n');
            text.push_str(line);
        }
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
    if let Some(line) = brief.staleness.line() {
        lines.push(line.to_string());
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
mod tests;
