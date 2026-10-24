// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Recipe steps and their ablation flags (`specs/brief.md`, "Recipes" and
//! "Ablation").
//!
//! A step is evidence a kind's recipe adds that Pixel first shipped as a
//! prototype script (a "probe"): it is written here as a typed function over
//! [`crate::chain::Evidence`], with the prototype's cases as its tests. Every
//! step has a flag: `PIXEL_BRIEF_STEPS=-name,+name` and `pixel brief --without
//! name` switch it off, so a replay can compare the brief with and without it
//! and a step that removes no tool call is deleted.
//!
//! The steps share one [`Ctx`] per brief: the typed text, the question kind,
//! the files the brief already points at, and the tracked-file listing, read
//! once and only if a step's lexical condition asks for it.

mod callers;
mod code_sites;
mod config_registry;
mod docs;
mod error_origin;
mod flow;
mod history;
mod names;
mod overview;
mod stop_cause;
mod test_bodies;

use std::cell::{OnceCell, RefCell};
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;

use crate::chain::Evidence;
use crate::region_pack::Block;
use crate::routes::QuestionKind;

/// The longest tracked file a step reads, in bytes.
const MAX_FILE_BYTES: u64 = 300_000;

/// `word` lowercased, with one plural or `-ing` ending cut.
pub(crate) fn stem(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in ["ing", "es", "s"] {
        if word.chars().count() > 4
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_string();
        }
    }
    word
}

/// The words of code or prose, split at case changes, digits and punctuation,
/// lowercased: `HTTPServerError2` is `http`, `server`, `error`, `2`. Mirrors
/// `[A-Z]+(?![a-z])|[A-Z]?[a-z]+|\d+`: a run of capitals stops before the
/// capital that starts the next word.
pub(crate) fn subwords(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let upper = |c: char| c.is_ascii_uppercase();
        let lower = |c: char| c.is_ascii_lowercase();
        let mut j = i;
        if upper(chars[i]) {
            while j < chars.len() && upper(chars[j]) {
                j += 1;
            }
            let followed_by_lower = j < chars.len() && lower(chars[j]);
            if !followed_by_lower {
                out.push(chars[i..j].iter().collect::<String>().to_lowercase());
                i = j;
                continue;
            }
            if j - i >= 2 {
                // `HTTPServer`: the capitals but the last, which starts `Server`.
                out.push(chars[i..j - 1].iter().collect::<String>().to_lowercase());
                i = j - 1;
                continue;
            }
        }
        j = i + usize::from(upper(chars[i]));
        let lowers_from = j;
        while j < chars.len() && lower(chars[j]) {
            j += 1;
        }
        if j > lowers_from {
            out.push(chars[i..j].iter().collect::<String>().to_lowercase());
            i = j;
        } else if chars[i].is_ascii_digit() {
            let mut k = i;
            while k < chars.len() && chars[k].is_ascii_digit() {
                k += 1;
            }
            out.push(chars[i..k].iter().collect());
            i = k;
        } else {
            i += 1;
        }
    }
    out
}

/// What the steps of one brief are given.
pub(crate) struct Ctx<'a> {
    typed: &'a str,
    kind: QuestionKind,
    steps: Steps,
    /// Paths the brief already points at: a file named by a prompt word is a
    /// lead only beside them.
    near: &'a [String],
    evidence: &'a dyn Evidence,
    deadline: Instant,
    held: Held,
    files: OnceCell<Option<Vec<String>>>,
    timings: RefCell<Vec<StepRun>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        typed: &'a str,
        kind: QuestionKind,
        steps: Steps,
        near: &'a [String],
        evidence: &'a dyn Evidence,
        deadline: Instant,
    ) -> Self {
        Self {
            typed,
            kind,
            steps,
            near,
            evidence,
            deadline,
            held: Held::default(),
            files: OnceCell::new(),
            timings: RefCell::new(Vec::new()),
        }
    }

    /// The tracked files, sorted; listed on first use. `None` when the source
    /// cannot list them.
    fn files(&self) -> Option<&[String]> {
        self.files
            .get_or_init(|| self.evidence.tracked_files(self.deadline).ok())
            .as_deref()
    }

    /// A tracked file's text, up to `max` bytes; `None` when it cannot be read.
    fn read_up_to(&self, path: &str, max: u64) -> Option<String> {
        self.evidence.read_file(path, max, self.deadline).ok()
    }

    /// `path`'s text under the default cap.
    fn read(&self, path: &str) -> Option<String> {
        self.read_up_to(path, MAX_FILE_BYTES)
    }

    /// Whether `step` runs for this question: its flag is on and the kind is
    /// not one of its `skip`.
    fn wants(&self, step: Step, skip: &[QuestionKind]) -> bool {
        self.steps.enabled(step) && !skip.contains(&self.kind)
    }

    fn timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Times `step` from here to the end of the scope that holds the timer: a
    /// step that returns before its lexical gate passed never starts one, so
    /// it is absent from the brief's record, not `0`. The tracked-file listing
    /// belongs to the first step that asks for it.
    fn timer(&self, step: Step) -> Timer<'_> {
        Timer {
            timings: &self.timings,
            step,
            started: Instant::now(),
        }
    }
}

/// How long one step ran in one brief.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepRun {
    pub step: Step,
    pub ms: u64,
}

/// Records a [`StepRun`] when it is dropped.
struct Timer<'a> {
    timings: &'a RefCell<Vec<StepRun>>,
    step: Step,
    started: Instant,
}

impl std::ops::Drop for Timer<'_> {
    fn drop(&mut self) {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.timings.borrow_mut().push(StepRun {
            step: self.step,
            ms,
        });
    }
}

/// Why a step takes blocks out of the rest of the pack: what it found makes
/// them noise. A rule reads a block's head, never its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Drop {
    /// Documentation (`.md`, `.rst`, man pages, `doc/` trees): the manual
    /// restates what the code the step found shows.
    Docs,
    /// The pack's `config registry` block.
    ConfigRegistry,
    /// A whole-file or file-named block of this path: the step shows the
    /// function that matters instead.
    WholeFile(String),
    /// Blocks outside this directory: the lexical neighbours of a question
    /// about one member are other members' files.
    Outside(String),
    /// Blocks whose head names none of these words.
    Unnamed(Vec<String>),
    /// A test block, or a list of tests, that mentions none of these names in
    /// its head or its lines: the question is about one function's callers,
    /// and the tests of its neighbours are noise.
    UnrelatedTest(Vec<String>),
    /// The pack's own call trace and call-graph facts: a flow step shows the
    /// chain that carries the question's words instead.
    CallTrace,
    /// Every block that is not about commits: a commit question is answered by
    /// the commit, and the code its words lead to is a neighbour's.
    AllButHistory,
    /// The definition blocks (`NAME definition (function)`) of every symbol
    /// but this one.
    OtherDefinitions(String),
    /// Every ranged block (`path:a-b — label`) whose label is not a
    /// definition: the step shows where the question's words meet instead.
    ///
    /// Pixel's `cand-j-stop-cause` kept the blocks whose label matched
    /// `definition|named by the prompt`. Here a definition is headed
    /// `NAME definition (kind)` (`region_pack::def_block`, also `… with its
    /// neighbours`), and `name-files` heads its own `NAME definition (named
    /// by the prompt: '…')` and `module named by the prompt`; the label
    /// test is the script's, applied to the text after the line range. A block
    /// without a `path:a-b` range (`config registry`, `call trace`) is not
    /// ranged and stays; `NAME region`, `NAME usage`, `whole file`, `NAME
    /// match` and `(a test of …)` blocks go.
    NotDefinitions,
}

static DOCS_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/")
        .expect("the documentation-head pattern is a literal")
});

static DEFINITION_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\S+?:\d+-\d+ — (\w+) definition \((?:method|function|variant|struct)\)$")
        .expect("the definition-head pattern is a literal")
});

static RANGED_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\S+?:\d+-\d+(.*)$").expect("the ranged-head pattern is a literal")
});

impl Drop {
    fn matches(&self, block: &Block) -> bool {
        let head = block.head();
        match self {
            Drop::Docs => DOCS_HEAD.is_match(head),
            Drop::ConfigRegistry => head.starts_with("config registry"),
            Drop::WholeFile(path) => head.strip_prefix(path.as_str()).is_some_and(|rest| {
                let Some(rest) = rest.strip_prefix(':') else {
                    return false;
                };
                let range = rest.split(" — ").next().unwrap_or("");
                let label = rest.split_once(" — ").map_or("", |(_, label)| label);
                !range.is_empty()
                    && range.chars().all(|ch| ch.is_ascii_digit() || ch == '-')
                    && (label.starts_with("file named") || label.starts_with("whole file"))
            }),
            Drop::Outside(dir) => !overview::inside(head, dir),
            Drop::Unnamed(words) => !words.iter().any(|word| head.contains(word.as_str())),
            Drop::UnrelatedTest(names) => {
                let is_test = head.contains("(test)")
                    || head.contains("(a test of ")
                    || head.starts_with("tests matching");
                is_test
                    && !names
                        .iter()
                        .any(|name| block.text().contains(name.as_str()))
            }
            Drop::CallTrace => {
                head.starts_with("call trace") || head.starts_with("call graph facts")
            }
            Drop::AllButHistory => !head.starts_with("history") && !head.starts_with("commit "),
            Drop::OtherDefinitions(name) => DEFINITION_HEAD
                .captures(head)
                .is_some_and(|caps| &caps[1] != name),
            Drop::NotDefinitions => RANGED_HEAD.captures(head).is_some_and(|caps| {
                !caps[1].contains("definition") && !caps[1].contains("named by the prompt")
            }),
        }
    }
}

/// The rules a step set: a block of the rest of the pack that any rule
/// matches is dropped. The steps' own blocks are never dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Drops(Vec<Drop>);

impl Drops {
    pub(crate) fn push(&mut self, rule: Drop) {
        if !self.0.contains(&rule) {
            self.0.push(rule);
        }
    }

    /// Whether the block is dropped.
    pub(crate) fn drops(&self, block: &Block) -> bool {
        self.0.iter().any(|rule| rule.matches(block))
    }

    /// Whether a block with this head and no lines is dropped.
    #[cfg(test)]
    pub(crate) fn drops_head(&self, head: &str) -> bool {
        self.drops(&Block::new(head.to_string(), Vec::new()))
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What the steps add to a pack.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Blocks that lead the pack.
    pub(crate) lead: Vec<Block>,
    /// Blocks that follow the pack's own.
    pub(crate) append: Vec<Block>,
    /// What the rest of the pack loses because of what the steps found.
    pub(crate) drops: Drops,
    /// A step that ran even though the relevance gate had closed the prompt
    /// (Pixel's `when_gate_closed`) produced a block.
    pub(crate) rescuing: bool,
    /// The steps that ran, in the order they finished.
    pub(crate) timings: Vec<StepRun>,
}

/// What the rest of the brief already holds, for the steps that fill a gap.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Held {
    /// The history index answered the question: commits are in the brief.
    pub(crate) history: bool,
}

/// The blocks of every step that applies to a `kind` question about `typed`;
/// `near` is the paths the brief already points at.
pub(crate) fn blocks(
    typed: &str,
    kind: QuestionKind,
    steps: Steps,
    near: &[String],
    evidence: &dyn Evidence,
    deadline: Instant,
) -> Outcome {
    blocks_held(
        typed,
        kind,
        steps,
        near,
        Held::default(),
        evidence,
        deadline,
    )
}

/// [`blocks`] for a brief that already holds `held`.
pub(crate) fn blocks_held(
    typed: &str,
    kind: QuestionKind,
    steps: Steps,
    near: &[String],
    held: Held,
    evidence: &dyn Evidence,
    deadline: Instant,
) -> Outcome {
    let mut ctx = Ctx::new(typed, kind, steps, near, evidence, deadline);
    ctx.held = held;
    let mut out = run(&ctx);
    out.timings = ctx.timings.take();
    out
}

fn run(ctx: &Ctx) -> Outcome {
    let mut out = Outcome::default();
    overview::run(ctx, &mut out);
    callers::run(ctx, &mut out);
    code_sites::run(ctx, &mut out);
    config_registry::run(ctx, &mut out);
    names::run(ctx, &mut out);
    test_bodies::run(ctx, &mut out);
    stop_cause::run(ctx, &mut out);
    flow::run(ctx, &mut out);
    history::run(ctx, &mut out);
    error_origin::run(ctx, &mut out);
    docs::run(ctx, &mut out);
    out
}

/// One step of a recipe, named as its prototype was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// One line per workspace member, or the named members' manifests.
    WorkspaceOverview,
    /// What a named Cargo crate exposes, depends on and includes.
    OverviewAnswerEvidence,
    /// A named member's manifest and every tracked file of its directory.
    MemberManifest,
    /// The callers of the function a question names, their callers, and the
    /// tests among them.
    CandJCallers,
    /// The functions that read a named variable, or that apply the flags a
    /// question asks about.
    CodeSites,
    /// The call chain between the entry and the type a trace question names.
    CandJChain,
    /// The code and the tests where the words of a "why does X stop" question
    /// meet.
    CandJStopCause,
    /// The callees of a named entry, ranked by the question's words.
    FlowRank,
    /// A history question answered from git when the brief has no commits.
    HistoryFallback,
    /// The message, files and patch of the commit a history question names.
    CommitPatch,
    /// The lines that can emit the error a question quotes or describes.
    ErrorOrigin,
    /// The registry of configuration keys a setting question asks about.
    ConfigRegistry,
    /// The test cases whose name or body carries a tests question's subject.
    TestBodies,
    /// The documentation sections whose heading names the question's words.
    DocSections,
    /// The pack's blocks that share no word with the question go, the rest
    /// move to the front by score.
    RankBlocks,
    /// Lists of test names, completion files, overlapping ranges, region lines
    /// another block shows and oversized blocks are trimmed.
    PruneNoise,
    /// A code name the adjacent words spell, the file a word names, and the
    /// module a question about parts and dependencies is about.
    NameFiles,
}

impl Step {
    /// Every step, in the order `pixel brief --help` lists them.
    pub const ALL: [Step; 17] = [
        Step::WorkspaceOverview,
        Step::OverviewAnswerEvidence,
        Step::MemberManifest,
        Step::CandJCallers,
        Step::CodeSites,
        Step::CandJChain,
        Step::CandJStopCause,
        Step::FlowRank,
        Step::HistoryFallback,
        Step::CommitPatch,
        Step::ErrorOrigin,
        Step::ConfigRegistry,
        Step::TestBodies,
        Step::DocSections,
        Step::NameFiles,
        Step::RankBlocks,
        Step::PruneNoise,
    ];

    /// The name a flag spells.
    pub const fn name(self) -> &'static str {
        match self {
            Step::WorkspaceOverview => "workspace-overview",
            Step::OverviewAnswerEvidence => "overview-answer-evidence",
            Step::MemberManifest => "cand-k-member-manifest",
            Step::CandJCallers => "cand-j-callers",
            Step::CodeSites => "cand-k-code-sites",
            Step::CandJChain => "cand-j-chain",
            Step::CandJStopCause => "cand-j-stop-cause",
            Step::FlowRank => "flow-rank",
            Step::HistoryFallback => "history-fallback",
            Step::CommitPatch => "cand-k-commit-patch",
            Step::ErrorOrigin => "error-origin",
            Step::ConfigRegistry => "config-registry",
            Step::TestBodies => "test-bodies",
            Step::DocSections => "doc-sections",
            Step::NameFiles => "name-files",
            Step::RankBlocks => "rank-blocks",
            Step::PruneNoise => "prune-noise",
        }
    }

    /// The step a flag names.
    ///
    /// # Errors
    ///
    /// A name that is no step, with the known ones.
    pub fn parse(name: &str) -> Result<Self, String> {
        Step::ALL
            .into_iter()
            .find(|step| step.name() == name)
            .ok_or_else(|| {
                let known: Vec<&str> = Step::ALL.into_iter().map(Step::name).collect();
                format!("unknown brief step \"{name}\"; known: {}", known.join(", "))
            })
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// Which steps run. Every step is on until a flag turns it off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Steps {
    off: u32,
}

impl Steps {
    /// Whether `step` runs.
    pub const fn enabled(self, step: Step) -> bool {
        self.off & step.bit() == 0
    }

    /// These steps with `spec` applied in order: `-name` turns a step off,
    /// `+name` back on; items are separated by commas or spaces.
    ///
    /// # Errors
    ///
    /// An item without a `-` or `+`, or naming no step.
    pub fn apply(self, spec: &str) -> Result<Self, String> {
        let mut steps = self;
        for item in spec
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .filter(|item| !item.is_empty())
        {
            let (on, name) = if let Some(name) = item.strip_prefix('+') {
                (true, name)
            } else if let Some(name) = item.strip_prefix('-') {
                (false, name)
            } else {
                return Err(format!("brief step \"{item}\" needs a + or - in front"));
            };
            let step = Step::parse(name)?;
            if on {
                steps.off &= !step.bit();
            } else {
                steps.off |= step.bit();
            }
        }
        Ok(steps)
    }

    /// These steps with every named one off.
    ///
    /// # Errors
    ///
    /// A name that is no step.
    pub fn without(self, names: &[String]) -> Result<Self, String> {
        let mut steps = self;
        for name in names {
            steps.off |= Step::parse(name)?.bit();
        }
        Ok(steps)
    }
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_steps_that_ran_are_timed_and_a_step_that_did_not_is_absent_not_zero() {
        use std::time::Duration;

        let repo = testing::Repo::new(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            (
                "crates/a/Cargo.toml",
                "[package]\nname = \"a\"\ndescription = \"A\"\n",
            ),
            (
                "crates/b/Cargo.toml",
                "[package]\nname = \"b\"\ndescription = \"B\"\n",
            ),
        ]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let ran = |typed: &str, kind: QuestionKind, steps: Steps| -> Vec<&'static str> {
            blocks(typed, kind, steps, &[], &repo, deadline)
                .timings
                .iter()
                .map(|run| run.step.name())
                .collect()
        };
        let asked = ran(
            "which crates make up the workspace",
            QuestionKind::Overview,
            Steps::default(),
        );
        assert!(asked.contains(&"workspace-overview"), "{asked:?}");
        assert!(
            !asked.contains(&"cand-j-callers") && !asked.contains(&"flow-rank"),
            "{asked:?}"
        );
        let off = Steps::default()
            .without(&["workspace-overview".to_string()])
            .unwrap();
        assert!(
            !ran(
                "which crates make up the workspace",
                QuestionKind::Overview,
                off
            )
            .contains(&"workspace-overview")
        );
        assert!(ran("thanks", QuestionKind::Lookup, Steps::default()).is_empty());
    }

    #[test]
    fn every_step_runs_until_a_flag_turns_it_off() {
        let steps = Steps::default();
        assert!(Step::ALL.into_iter().all(|step| steps.enabled(step)));
    }

    #[test]
    fn a_spec_applies_in_order_and_a_later_plus_wins() {
        let steps = Steps::default()
            .apply("-workspace-overview, -cand-k-member-manifest,+workspace-overview")
            .unwrap();
        assert!(steps.enabled(Step::WorkspaceOverview));
        assert!(!steps.enabled(Step::MemberManifest));
        assert!(steps.enabled(Step::OverviewAnswerEvidence));
    }

    #[test]
    fn without_turns_the_named_steps_off_and_refuses_an_unknown_one() {
        let steps = Steps::default()
            .without(&["overview-answer-evidence".to_string()])
            .unwrap();
        assert!(!steps.enabled(Step::OverviewAnswerEvidence));
        assert!(steps.enabled(Step::WorkspaceOverview));
        let error = Steps::default()
            .without(&["no-such-step".to_string()])
            .unwrap_err();
        assert_eq!(
            error,
            "unknown brief step \"no-such-step\"; known: workspace-overview, \
             overview-answer-evidence, cand-k-member-manifest, cand-j-callers, cand-k-code-sites, cand-j-chain, cand-j-stop-cause, flow-rank, history-fallback, cand-k-commit-patch, error-origin, config-registry, test-bodies, doc-sections, name-files, rank-blocks, prune-noise"
        );
    }

    #[test]
    fn a_bare_name_in_a_spec_is_refused() {
        assert_eq!(
            Steps::default().apply("workspace-overview").unwrap_err(),
            "brief step \"workspace-overview\" needs a + or - in front"
        );
        assert_eq!(Steps::default().apply("").unwrap(), Steps::default());
    }
}
