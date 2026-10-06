// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt-start evidence brief: a bounded chain of Pixel lookups the
//! product runs itself, rendered as one compact `[PIXEL:BRIEF]` block.
//!
//! The chain is `search-content` on the first anchor, `find-code` when that
//! found no file, `find-symbol` for a uid, and `impact` on that uid when the
//! prompt asks about a change or its callers. It shares one deadline and at
//! most [`MAX_OPS`] operations, runs on its own thread, and renders whatever
//! it has when the deadline passes, so it never holds the prompt back. The
//! lookups sit behind [`Evidence`]: `evidence.rs` is the live source, tests
//! bring their own.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use super::{SOURCE_EXTENSIONS, names_code, retrieval_request};

/// Opening line of the block; a host and a test find the brief by it.
pub(crate) const BRIEF_TAG: &str = "[PIXEL:BRIEF]";
/// Settings key of the opt-out (`brief: false` in `.pixel/config.yaml`).
pub(crate) const BRIEF_FEATURE: &str = "brief";
/// Environment opt-out: `0`, `false` or `off` silences the brief.
pub(crate) const BRIEF_ENV: &str = "PIXEL_BRIEF";
/// One window shared by every operation of one brief, measured from the
/// moment the hook receives the prompt.
pub(crate) const BRIEF_WINDOW: Duration = Duration::from_millis(750);
/// Operations one brief may start, whatever they answer.
pub(crate) const MAX_OPS: usize = 4;
/// Rendered size cap; lists give way before a line is cut.
pub(crate) const BRIEF_BYTES: usize = 2048;
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
const MAX_CONCEPT_WORDS: usize = 6;
const MIN_CONCEPT_WORD_CHARS: usize = 4;
const MAX_ITEM_CHARS: usize = 120;

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
    /// Files a free-text concept points at.
    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String>;
    /// Declarations named `name`.
    fn symbols(&self, name: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String>;
    /// Direct callers (impact depth 1) of a uid, or of a bare name.
    fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String>;
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

    fn names(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .map(String::as_str)
            .filter(|anchor| !is_path(anchor))
    }

    fn paths(&self) -> Vec<&str> {
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
}

/// Everything the chain needs from the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    anchors: Anchors,
    change_intent: bool,
    concept: Option<String>,
}

impl Plan {
    /// `None` for a prompt that does not ask about code, pasted text aside, and
    /// for a continuation or a harness envelope, which are not the user's task.
    pub(crate) fn from_prompt(prompt: &str) -> Option<Self> {
        if crate::prompt_submit::is_trivial_continuation(prompt) {
            return None;
        }
        let typed = retrieval_request(prompt)?;
        Some(Self {
            anchors: Anchors::from_text(&typed),
            change_intent: has_change_intent(&typed),
            concept: concept_phrase(&typed),
        })
    }
}

/// The prompt asks about a change or about who depends on something.
pub(crate) fn has_change_intent(typed: &str) -> bool {
    let lower = typed.to_lowercase();
    CHANGE_STEMS.iter().any(|stem| lower.contains(stem))
}

/// The first words of the prompt that say what it is about.
fn concept_phrase(typed: &str) -> Option<String> {
    let words: Vec<String> = WORD
        .find_iter(typed)
        .map(|word| word.as_str().to_lowercase())
        .filter(|word| {
            word.chars().count() >= MIN_CONCEPT_WORD_CHARS
                && !CONCEPT_STOPWORDS.contains(&word.as_str())
        })
        .take(MAX_CONCEPT_WORDS)
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
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

/// What the chain has learned, shared between the worker and the hook.
#[derive(Clone, Debug, Default)]
pub(crate) struct Brief {
    anchors: Vec<String>,
    files: Vec<FileHit>,
    defined: Vec<SymbolHit>,
    callers: Vec<CallerHit>,
    excluded: Vec<String>,
    unresolved: Vec<String>,
    ops: usize,
    answered: usize,
    searched: bool,
    impacted: bool,
    cut: bool,
    finished: bool,
}

impl Brief {
    fn absorb(&mut self, found: Found) {
        for hit in found.hits {
            if is_generated(&hit.path) {
                if !self.excluded.contains(&hit.path) {
                    self.excluded.push(hit.path);
                }
            } else if !self.files.iter().any(|file| file.path == hit.path) {
                self.files.push(hit);
            }
        }
        if found.capped {
            self.unresolved.push(format!(
                "search stopped at {SEARCH_ROWS} rows, the file list is a prefix"
            ));
        }
    }

    fn confidence(&self) -> &'static str {
        if !self.callers.is_empty() {
            "high"
        } else if !self.files.is_empty() || !self.defined.is_empty() {
            "medium"
        } else {
            "low"
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
    edit(state, |brief| brief.anchors.clone_from(&plan.anchors.0));
    if let Some(term) = plan.anchors.search_term()
        && spend(state, deadline)
    {
        match evidence.files_with(&term, deadline) {
            Ok(found) => edit(state, |brief| {
                brief.answered += 1;
                brief.searched = true;
                brief.absorb(found);
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
        match evidence.concept(phrase, deadline) {
            Ok(found) => edit(state, |brief| {
                brief.answered += 1;
                brief.searched = true;
                brief.absorb(found);
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
                edit(state, |brief| {
                    brief.answered += 1;
                    brief.defined = ordered(&hits, pick);
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
    edit(state, |brief| brief.finished = true);
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
    start_with(prompt, Gate::read(&root), BRIEF_WINDOW, move |deadline| {
        super::evidence::open(&root, deadline)
    })
}

/// [`start`] with its gate, window and evidence source given.
pub(crate) fn start_with<F>(prompt: &str, gate: Gate, window: Duration, open: F) -> Option<Pending>
where
    F: FnOnce(Instant) -> Box<dyn Evidence> + Send + 'static,
{
    if !gate.open() {
        return None;
    }
    let plan = Plan::from_prompt(prompt)?;
    let deadline = Instant::now() + window;
    let state = Arc::new(Mutex::new(Brief::default()));
    let (finished, done) = mpsc::channel();
    let worker = Arc::clone(&state);
    std::thread::Builder::new()
        .name("pixel-brief".into())
        .spawn(move || {
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
    excluded: usize,
    unresolved: usize,
}

impl Shown {
    fn of(brief: &Brief) -> Self {
        Self {
            files: brief.files.len().min(MAX_FILES),
            defined: brief.defined.len().min(MAX_DEFINED),
            callers: brief.callers.len().min(MAX_CALLERS),
            excluded: brief.excluded.len(),
            unresolved: brief.unresolved.len(),
        }
    }

    /// Drop one entry from the longest list (the earlier of equals in the
    /// order excluded, files, callers, defined, unresolved); `false` when
    /// every list is already empty.
    fn shrink(&mut self) -> bool {
        let widest = [
            self.excluded,
            self.files,
            self.callers,
            self.defined,
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
            &mut self.callers,
            &mut self.defined,
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
    if !brief.anchors.is_empty() {
        let anchors: Vec<String> = brief.anchors.iter().map(|a| clean(a)).collect();
        lines.push(format!("anchors: {}", anchors.join(", ")));
    }
    if let Some(line) = list_line(
        "defined",
        brief.defined.iter().map(|hit| {
            format!(
                "{} {} {}:{}-{}",
                clean(&hit.kind),
                clean(&hit.name),
                clean(&hit.path),
                hit.start_line,
                hit.end_line
            )
        }),
        shown.defined,
        "; ",
        false,
    ) {
        lines.push(line);
    }
    if let Some(line) = list_line(
        "files",
        brief
            .files
            .iter()
            .map(|hit| format!("{}:{}", clean(&hit.path), hit.line)),
        shown.files,
        " ",
        brief.searched,
    ) {
        lines.push(line);
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
        "confidence: {} | ops: {}/{MAX_OPS}{partial}",
        brief.confidence(),
        brief.ops
    ));
    lines.push(FOOTER.to_string());
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
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(MAX_ITEM_CHARS)
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
    fn concept_phrase_should_skip_short_and_common_words_and_cap_the_count() {
        assert_eq!(
            concept_phrase("how does the session cache expire stale entries after logout today"),
            Some("session cache expire stale entries after".to_string())
        );
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
        concept: Result<Found, String>,
        symbols: Result<Vec<SymbolHit>, String>,
        callers: Result<Vec<CallerHit>, String>,
        pause: Duration,
        log: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                files: Ok(Found::default()),
                concept: Ok(Found::default()),
                symbols: Ok(Vec::new()),
                callers: Ok(Vec::new()),
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
        fn concept(&self, phrase: &str, _: Instant) -> Result<Found, String> {
            self.note(format!("concept {phrase}"));
            self.concept.clone()
        }
        fn symbols(&self, name: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            self.note(format!("symbols {name}"));
            self.symbols.clone()
        }
        fn callers(&self, target: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            self.note(format!("callers {target}"));
            self.callers.clone()
        }
    }

    fn chain(prompt: &str, fake: &Fake, window: Duration) -> Brief {
        let plan = Plan::from_prompt(prompt).unwrap();
        let state = Mutex::new(Brief::default());
        run(&plan, fake, &state, Instant::now() + window);
        state.into_inner().unwrap()
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
                "callers packages/ui/handleError.ts#handleError#function"
            ]
        );
        assert_eq!(brief.files, [hit("apps/web/page.tsx", 4)]);
        assert_eq!(brief.excluded, ["data/out.json"]);
        assert_eq!(brief.callers, [caller("apps/web/page.tsx", "Page", 12)]);
        assert_eq!((brief.ops, brief.answered), (3, 3));
        assert!(brief.finished && !brief.cut);
        assert_eq!(brief.confidence(), "high");
    }

    #[test]
    fn chain_should_skip_impact_for_a_literal_lookup() {
        let mut fake = Fake::new();
        fake.files = Ok(found(vec![hit("src/a.ts", 2)]));
        fake.symbols = Ok(vec![symbol("src/a.ts", "fetchUser")]);
        let brief = chain("where is fetchUser defined", &fake, SECOND);
        assert_eq!(fake.calls(), ["files_with fetchUser", "symbols fetchUser"]);
        assert_eq!(brief.ops, 2);
        assert!(brief.callers.is_empty());
        assert!(!brief.impacted);
        assert_eq!(brief.confidence(), "medium");
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
                "callers fetchUser"
            ]
        );
        assert_eq!(
            brief.unresolved,
            ["find-symbol fetchUser: no uid, bare name used"]
        );
        assert_eq!(brief.ops, MAX_OPS);
        assert_eq!(brief.confidence(), "low");
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
        assert_eq!(brief.files, [hit("src/retry.ts", 7)]);
    }

    #[test]
    fn chain_should_note_a_failed_operation_and_not_count_it_as_an_answer() {
        let mut fake = Fake::new();
        fake.files = Err("text index is not current".into());
        fake.concept = Err("needs a running daemon".into());
        fake.symbols = Err("graph is stale".into());
        let brief = chain("callers of `fetchUser`", &fake, SECOND);
        assert_eq!(brief.answered, 0);
        assert_eq!(brief.ops, 3);
        assert_eq!(
            brief.unresolved,
            [
                "search fetchUser: text index is not current",
                "find-code: needs a running daemon",
                "find-symbol fetchUser: graph is stale"
            ]
        );
        assert_eq!(render(&brief), None);
        assert!(!fake.calls().iter().any(|call| call.starts_with("callers")));
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
            text.contains("confidence: medium | ops: 1/4 | partial: budget"),
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
            text.contains("confidence: medium | ops: 2/4 | partial: budget"),
            "{text}"
        );
    }

    #[test]
    fn start_should_decline_when_switched_off_unindexed_or_not_about_code() {
        let open = |_: Instant| -> Box<dyn Evidence> { Box::new(Fake::new()) };
        assert!(start_with("callers of `fetchUser`", OPEN, SECOND, open).is_some());
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
            assert!(start_with(prompt, gate, SECOND, open).is_none(), "{prompt}");
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
            files: vec![hit("apps/web/page.tsx", 4), hit("apps/web/other.tsx", 9)],
            defined: vec![symbol("src/handleError.ts", "handleError")],
            callers: vec![caller("apps/web/page.tsx", "Page", 12)],
            excluded: vec!["data/out.json".into()],
            unresolved: vec!["find-symbol handleError: 2 candidates, took first".into()],
            ops: 3,
            answered: 3,
            searched: true,
            impacted: true,
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
                "anchors: handleError, src/handleError.ts",
                "defined: function handleError src/handleError.ts:3-9",
                "files: apps/web/page.tsx:4 apps/web/other.tsx:9",
                "callers (impact d1): apps/web/page.tsx -> Page:12",
                "excluded (generated): data/out.json",
                "unresolved: find-symbol handleError: 2 candidates, took first",
                "confidence: high | ops: 3/4",
                FOOTER,
            ]
            .join("\n")
        );
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
        assert!(text.contains("confidence: low | ops: 1/4\n"), "{text}");
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
            .map(|n| hit(&format!("apps/web/some/deep/dir/component_{n}.tsx"), n))
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
        assert_eq!(text.lines().last(), Some(FOOTER));
    }

    #[test]
    fn render_should_drop_whole_entries_until_it_fits_and_stop_as_soon_as_it_does() {
        let mut brief = full_brief();
        brief.files = (0..8)
            .map(|n| hit(&format!("{}{n}", "d".repeat(MAX_ITEM_CHARS - 1)), 1))
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
        assert!(text.contains("confidence: high | ops: 3/4"), "{text}");
        assert!(text.ends_with(FOOTER), "{text}");
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
            excluded: 1,
            unresolved: 1,
        };
        let mut order = Vec::new();
        while shown.shrink() {
            order.push((
                shown.excluded,
                shown.files,
                shown.callers,
                shown.defined,
                shown.unresolved,
            ));
        }
        assert_eq!(
            order,
            [
                (0, 1, 1, 1, 1),
                (0, 0, 1, 1, 1),
                (0, 0, 0, 1, 1),
                (0, 0, 0, 0, 1),
                (0, 0, 0, 0, 0)
            ]
        );
        let mut uneven = Shown {
            files: 2,
            defined: 1,
            callers: 5,
            excluded: 0,
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
    fn absorb_should_split_generated_files_dedupe_and_flag_a_capped_search() {
        let mut brief = Brief::default();
        brief.absorb(Found {
            hits: vec![
                hit("src/a.ts", 3),
                hit("src/a.ts", 9),
                hit("package.json", 1),
                hit("package.json", 4),
            ],
            capped: true,
        });
        assert_eq!(brief.files, [hit("src/a.ts", 3)]);
        assert_eq!(brief.excluded, ["package.json"]);
        assert_eq!(
            brief.unresolved,
            ["search stopped at 200 rows, the file list is a prefix"]
        );
    }

    #[test]
    fn confidence_should_follow_callers_then_files_then_declarations() {
        let mut brief = Brief::default();
        assert_eq!(brief.confidence(), "low");
        brief.defined = vec![symbol("a.ts", "go")];
        assert_eq!(brief.confidence(), "medium");
        brief.defined.clear();
        brief.files = vec![hit("a.ts", 1)];
        assert_eq!(brief.confidence(), "medium");
        brief.callers = vec![caller("b.ts", "f", 1)];
        assert_eq!(brief.confidence(), "high");
    }
}
