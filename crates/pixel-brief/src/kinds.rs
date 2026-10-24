// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The packs of the question kinds that name no symbol: how the repository is
//! organised (a map, the head of its architecture document, its manifest), a
//! review of a change (what the commit or the diff touched, the code around
//! it, who calls it), and a walkthrough (the calls out of the entry point, in
//! call order, with their bodies).
//!
//! A census of vanilla sessions found these questions answered from the same
//! few reads (list the tree, open the architecture document; `git show`, then
//! the functions it touched; grep the handler, then read what it calls) and
//! the relevance gate closed on all three, because they are not phrased in the
//! repository's words. `routes::QuestionKind` classifies the prompt without a
//! model and the gate lets those kinds in; this file turns the evidence into
//! the numbered blocks of the region pack. The chain gathers it behind
//! [`Evidence`] (the graph's files and edges, one bounded git call, file
//! reads); what decides what is shown is a pure function here: how files are
//! grouped into areas, which declarations stand for an area, which lines of a
//! changed file are worth reading, how a call tree is listed.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::answer::{self, Receipt};
use crate::chain::{Evidence, HistoryHit, SymbolHit};
use crate::locator;
use crate::region_pack::{self, Block, DefSite};
use crate::routes::{QuestionKind, has_phrase, plain};

// ---------------------------------------------------------------------------
// What the evidence is made of
// ---------------------------------------------------------------------------

/// One indexed file with the number of symbols the graph holds for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapFile {
    pub path: String,
    pub lang: String,
    pub symbols: usize,
}

/// A declaration of the top level of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopSymbol {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: u64,
    pub end: u64,
    pub sig: String,
}

/// The repository as the code graph has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoMap {
    pub files: Vec<MapFile>,
    pub symbols: usize,
    pub tops: Vec<TopSymbol>,
    /// Functions called `main`.
    pub mains: Vec<TopSymbol>,
}

/// Which change a review is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeScope {
    /// The commit at HEAD.
    Latest,
    /// The commit with this abbreviated or full hex id.
    Commit(String),
    /// Everything since the branch left the default branch.
    Branch,
    /// The working tree against HEAD.
    Working,
}

/// One file of a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// `A`dded, `D`eleted or `M`odified.
    pub status: char,
    pub added: usize,
    pub removed: usize,
    /// The lines of the new side that changed, `(first, last)`, in order.
    pub hunks: Vec<(u64, u64)>,
    /// The diff itself: each hunk header and its `+`/`-` lines, cut at
    /// [`MAX_PATCH_LINES`] lines.
    pub patch: Vec<String>,
}

/// Lines of a file's diff kept.
pub const MAX_PATCH_LINES: usize = 120;

/// A change: what it is, and its files, largest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    pub label: String,
    /// `sha subject` of the commit, when the change is one.
    pub subject: Option<String>,
    pub files: Vec<ChangedFile>,
    /// More files changed than the list holds.
    pub truncated: bool,
}

/// One call of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowNode {
    pub depth: usize,
    /// The index of the caller in the list.
    pub parent: Option<usize>,
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start: u64,
    pub end: u64,
}

// ---------------------------------------------------------------------------
// The overview: areas, their faces, the documents
// ---------------------------------------------------------------------------

/// An area is split into its sub-directories when it holds more files.
const SPLIT_FILES: usize = 60;
/// Directory levels an area is split to.
const MAX_DEPTH: usize = 3;
/// Areas listed at most.
const MAX_AREAS: usize = 30;
/// Declarations that stand for an area, and for a file in a small repository.
const PER_AREA: usize = 4;
const PER_SMALL_FILE: usize = 4;
/// A repository this small is listed file by file.
const SMALL_REPO_FILES: usize = 60;
/// Entry points listed.
const MAX_MAINS: usize = 6;
/// Lines of the architecture document, of the README beside it, of the
/// README alone, and of the manifest.
const ARCH_LINES: u64 = 70;
const README_LINES_BESIDE: u64 = 35;
const README_LINES: u64 = 70;
const MANIFEST_LINES: u64 = 45;
/// Lines of the architecture document read for its outline, and headings
/// listed.
const OUTLINE_SCAN: u64 = 800;
const OUTLINE_HEADINGS: usize = 40;

const ARCHITECTURE_DOCS: &[&str] = &[
    "ARCHITECTURE.md",
    "docs/ARCHITECTURE.md",
    "docs/architecture.md",
    "ARCHITECTURE.rst",
    "docs/overview.md",
];
const README_DOCS: &[&str] = &[
    "README.md",
    "Readme.md",
    "readme.md",
    "README.rst",
    "README.txt",
    "README",
];
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "pyproject.toml",
    "package.json",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "Gemfile",
    "setup.py",
];

/// A set of files that share a directory prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Area {
    /// The directory, without a trailing slash; empty for the root.
    pub(crate) prefix: String,
    /// Indices into the file list.
    pub(crate) files: Vec<usize>,
}

/// The files grouped by directory: one area for the whole when it is small,
/// otherwise the directories, split further while an area holds more than
/// [`SPLIT_FILES`] files and [`MAX_DEPTH`] levels allow.
pub(crate) fn split_areas(files: &[MapFile]) -> Vec<Area> {
    fn split(prefix: &str, indices: Vec<usize>, files: &[MapFile], depth: usize) -> Vec<Area> {
        if depth >= MAX_DEPTH || indices.len() <= SPLIT_FILES {
            return vec![Area {
                prefix: prefix.to_string(),
                files: indices,
            }];
        }
        let skip = if prefix.is_empty() {
            0
        } else {
            prefix.len() + 1
        };
        let mut direct: Vec<usize> = Vec::new();
        let mut children: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for index in indices {
            match files[index].path[skip..].split_once('/') {
                Some((child, _)) => children.entry(child.to_string()).or_default().push(index),
                None => direct.push(index),
            }
        }
        let mut areas = Vec::new();
        if !direct.is_empty() {
            areas.push(Area {
                prefix: prefix.to_string(),
                files: direct,
            });
        }
        for (child, members) in children {
            let path = if prefix.is_empty() {
                child
            } else {
                format!("{prefix}/{child}")
            };
            areas.extend(split(&path, members, files, depth + 1));
        }
        areas
    }
    split("", (0..files.len()).collect(), files, 0)
}

/// How much a declaration says about what its module is for: a type before a
/// function before a module, a public one before a private one, a longer one
/// before a shorter.
fn face_score(top: &TopSymbol) -> u64 {
    let kind = match top.kind.as_str() {
        "class" | "struct" | "enum" | "trait" | "interface" => 3,
        "function" => 2,
        _ => 1,
    };
    let public = top.sig.starts_with("pub")
        || top.sig.starts_with("export")
        || top.sig.starts_with("public")
        || (!top.sig.contains("fn ") && !top.name.starts_with('_'));
    kind * 4 + if public { 3 } else { 0 } + (top.end.saturating_sub(top.line) / 20).min(5)
}

/// The word a listing gives a declaration kind.
fn kind_word(kind: &str) -> &str {
    match kind {
        "function" => "fn",
        "module" => "mod",
        other => other,
    }
}

/// `path:line kind name`: what a reader can open.
fn face(top: &TopSymbol) -> String {
    format!(
        "{}:{} {} {}",
        top.path,
        top.line,
        kind_word(&top.kind),
        top.name
    )
}

/// The `limit` declarations of `tops` that stand for them, best first, shown
/// in file order.
pub(crate) fn faces<'a>(tops: &[&'a TopSymbol], limit: usize) -> Vec<&'a TopSymbol> {
    let mut ranked: Vec<&TopSymbol> = tops
        .iter()
        .copied()
        .filter(|top| !answer::is_test_path(&top.path))
        .collect();
    ranked.sort_by(|a, b| {
        face_score(b)
            .cmp(&face_score(a))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    ranked.truncate(limit);
    ranked.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
    ranked
}

/// The languages of the files, most files first.
fn languages(files: &[MapFile]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for file in files {
        *counts.entry(file.lang.as_str()).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(lang, count)| (lang.to_string(), count))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Directories whose files are not the product: documentation, scripts,
/// evaluations, examples, benchmarks, fixtures, vendored code.
const AUXILIARY: &[&str] = &[
    "docs",
    "doc",
    "scripts",
    "script",
    "eval",
    "evals",
    "fuzz",
    "examples",
    "example",
    "benches",
    "bench",
    "benchmarks",
    "fixtures",
    "tools",
    "ci",
    "website",
    "assets",
    "vendor",
    "third_party",
];
/// Auxiliary areas named in the one line that sums them up.
const AUXILIARY_SHOWN: usize = 10;

/// Whether `path` belongs to the product: not a test, not under a hidden or
/// auxiliary directory.
fn is_primary(path: &str) -> bool {
    !answer::is_test_path(path)
        && !path
            .split('/')
            .any(|part| part.starts_with('.') || AUXILIARY.contains(&part))
}

/// The lines of the repository map: the languages, then the areas (or, for a
/// small repository, the files) each with the declarations that stand for it,
/// the tests and auxiliary directories in one line each, and the entry
/// points.
pub(crate) fn map_lines(map: &RepoMap) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let langs: Vec<String> = languages(&map.files)
        .into_iter()
        .take(4)
        .map(|(lang, count)| format!("{lang} {count}"))
        .collect();
    lines.push(format!(
        "{} indexed files, {} symbols ({})",
        map.files.len(),
        map.symbols,
        langs.join(", ")
    ));
    let primary: Vec<MapFile> = map
        .files
        .iter()
        .filter(|file| is_primary(&file.path))
        .cloned()
        .collect();
    let mut by_path: BTreeMap<&str, Vec<&TopSymbol>> = BTreeMap::new();
    for top in &map.tops {
        by_path.entry(top.path.as_str()).or_default().push(top);
    }
    if primary.len() <= SMALL_REPO_FILES {
        let mut bare: Vec<&str> = Vec::new();
        for file in &primary {
            let tops = by_path.get(file.path.as_str()).cloned().unwrap_or_default();
            let shown: Vec<String> = faces(&tops, PER_SMALL_FILE).into_iter().map(face).collect();
            if shown.is_empty() {
                bare.push(&file.path);
            } else {
                lines.push(format!(
                    "{} ({} symbols): {}",
                    file.path,
                    file.symbols,
                    shown.join("; ")
                ));
            }
        }
        if !bare.is_empty() {
            lines.push(format!("no declarations of their own: {}", bare.join(", ")));
        }
    } else {
        let mut areas = split_areas(&primary);
        areas.sort_by(|a, b| {
            b.files
                .len()
                .cmp(&a.files.len())
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        let hidden: Vec<Area> = areas.split_off(areas.len().min(MAX_AREAS));
        areas.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        for area in &areas {
            let members: Vec<&MapFile> = area.files.iter().map(|&i| &primary[i]).collect();
            let symbols: usize = members.iter().map(|file| file.symbols).sum();
            let tops: Vec<&TopSymbol> = members
                .iter()
                .flat_map(|file| {
                    by_path
                        .get(file.path.as_str())
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect();
            let lang = languages(&members.iter().map(|&file| file.clone()).collect::<Vec<_>>())
                .first()
                .map(|(lang, _)| lang.clone())
                .unwrap_or_default();
            let name = if area.prefix.is_empty() {
                "./".to_string()
            } else {
                format!("{}/", area.prefix)
            };
            let shown: Vec<String> = faces(&tops, PER_AREA).into_iter().map(face).collect();
            lines.push(format!(
                "{name} — {} files, {symbols} symbols, {lang}{}{}",
                area.files.len(),
                if shown.is_empty() { "" } else { ": " },
                shown.join("; ")
            ));
        }
        if !hidden.is_empty() {
            let files: usize = hidden.iter().map(|area| area.files.len()).sum();
            lines.push(format!("… {} more areas ({files} files)", hidden.len()));
        }
    }
    // Tests and auxiliary directories: where they are, not what is in them.
    let mut other: BTreeMap<String, usize> = BTreeMap::new();
    for file in map.files.iter().filter(|file| !is_primary(&file.path)) {
        let mut parts = file.path.split('/');
        let first = parts.next().unwrap_or("");
        let key = if parts.next().is_some() {
            format!("{first}/")
        } else {
            first.to_string()
        };
        *other.entry(key).or_default() += 1;
    }
    if !other.is_empty() {
        let mut ranked: Vec<(String, usize)> = other.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let shown: Vec<String> = ranked
            .iter()
            .take(AUXILIARY_SHOWN)
            .map(|(key, count)| format!("{key} ({count})"))
            .collect();
        lines.push(format!(
            "tests, docs, scripts and the like: {}{}",
            shown.join(", "),
            if ranked.len() > AUXILIARY_SHOWN {
                ", …"
            } else {
                ""
            }
        ));
    }
    let mains: Vec<String> = map
        .mains
        .iter()
        .filter(|top| is_primary(&top.path))
        .take(MAX_MAINS)
        .map(face)
        .collect();
    if !mains.is_empty() {
        lines.push(format!("entry points: {}", mains.join("; ")));
    }
    lines
}

/// The headings (`#` to `###`) of a markdown or reStructuredText-free
/// document, as `path:line heading`.
pub(crate) fn outline(path: &str, lines: &[String]) -> Vec<String> {
    let mut headings: Vec<String> = Vec::new();
    let mut fenced = false;
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let hashes = line.chars().take_while(|&ch| ch == '#').count();
        if !fenced && (1..=3).contains(&hashes) && line[hashes..].starts_with(' ') {
            headings.push(format!("{path}:{} {}", index + 1, line.trim_end()));
        }
    }
    headings.truncate(OUTLINE_HEADINGS);
    headings
}

/// The first existing file among `candidates`, with its first `count` lines.
fn first_of(
    evidence: &dyn Evidence,
    candidates: &[&str],
    count: u64,
    deadline: Instant,
) -> Option<(String, Vec<String>)> {
    candidates.iter().find_map(|path| {
        let lines = evidence.lines_at(path, 1, count, deadline).ok()?;
        (!lines.is_empty()).then(|| ((*path).to_string(), lines))
    })
}

/// The blocks of an overview: the map, the architecture document's head and
/// outline, the manifest, the README.
fn overview_blocks(evidence: &dyn Evidence, deadline: Instant) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    if let Ok(map) = evidence.repo_map(deadline)
        && !map.files.is_empty()
    {
        blocks.push(Block::new("repo map".to_string(), map_lines(&map)));
    }
    let architecture = first_of(evidence, ARCHITECTURE_DOCS, OUTLINE_SCAN, deadline);
    if let Some((path, lines)) = &architecture {
        let head: Vec<String> = lines.iter().take(ARCH_LINES as usize).cloned().collect();
        if let Some(block) = region_pack::range_block(
            "architecture document".to_string(),
            path,
            &head,
            1,
            head.len() as u64,
        ) {
            blocks.push(block);
        }
        let headings = outline(path, lines);
        if headings.len() > 2 {
            blocks.push(Block::new(
                "architecture document outline".to_string(),
                headings,
            ));
        }
    }
    if let Some((path, lines)) = first_of(evidence, MANIFESTS, MANIFEST_LINES, deadline)
        && let Some(block) = region_pack::range_block(
            "project manifest".to_string(),
            &path,
            &lines,
            1,
            lines.len() as u64,
        )
    {
        blocks.push(block);
    }
    let readme_lines = if architecture.is_some() {
        README_LINES_BESIDE
    } else {
        README_LINES
    };
    if let Some((path, lines)) = first_of(evidence, README_DOCS, readme_lines, deadline)
        && let Some(block) =
            region_pack::range_block("readme".to_string(), &path, &lines, 1, lines.len() as u64)
    {
        blocks.push(block);
    }
    blocks
}

// ---------------------------------------------------------------------------
// The review: the change, the code it touched, who calls it
// ---------------------------------------------------------------------------

/// Changed files listed in the summary.
const SUMMARY_FILES: usize = 40;
/// Files whose changed code is shown, and regions per file and in all; a test
/// file shows less, since a reader of a review wants the code under test.
const REVIEW_FILES: usize = 8;
const REGIONS_PER_FILE: usize = 2;
const TEST_REGIONS_PER_FILE: usize = 1;
const MAX_REGIONS: usize = 8;
const MAX_TEST_REGIONS: usize = 3;
/// Lines of one changed region, at most.
const REGION_LINES: u64 = 60;
/// Files asked for their declarations, touched symbols listed, and symbols
/// whose callers are listed with the callers shown for each.
const TOUCHED_FILES: usize = 4;
const MAX_TOUCHED: usize = 12;
const CALLER_SYMBOLS: usize = 4;
const CALLERS_EACH: usize = 3;
/// Unpaired source files named.
const UNPAIRED_SHOWN: usize = 8;

/// Which change the prompt is about. The words of the prompt decide; the
/// default is the latest commit, and the caller falls back to it when the
/// scope asked for holds nothing.
pub(crate) fn review_scope(typed: &str) -> ChangeScope {
    let text = plain(typed);
    let any = |list: &[&str]| list.iter().any(|phrase| has_phrase(&text, phrase));
    if any(&[
        "latest commit",
        "last commit",
        "most recent commit",
        "previous commit",
        "this commit",
        "the commit",
        "my commit",
        "head~1",
    ]) {
        ChangeScope::Latest
    } else if any(&["pull request", "this pr", "this branch", "my branch"]) {
        ChangeScope::Branch
    } else if any(&[
        "uncommitted",
        "unstaged",
        "staged",
        "working tree",
        "my changes",
        "these changes",
        "the changes",
        "the diff",
        "my diff",
        "this diff",
        "what i changed",
        "my work",
        "my patch",
        "this change",
    ]) {
        ChangeScope::Working
    } else {
        ChangeScope::Latest
    }
}

static COMMIT_ID: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b[0-9a-f]{7,40}\b").expect("the commit-id pattern is a literal")
});
static QUOTED_TITLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"["'`]([^"'`]{8,120})["'`]"#)
        .expect("the quoted-title pattern is a literal")
});

/// The commit a review names: an abbreviated or full hex id in the prompt (it
/// must hold a digit, so a word like `defaced` is not one), else a quoted
/// title found in the log by its subject. `None` when the prompt names none:
/// the words then pick the scope ([`review_scope`]).
pub(crate) fn named_commit(
    typed: &str,
    evidence: &dyn Evidence,
    deadline: Instant,
) -> Option<String> {
    if let Some(id) = COMMIT_ID.find_iter(typed).map(|m| m.as_str()).find(|id| {
        id.chars().any(|c| c.is_ascii_digit()) && id.chars().any(|c| c.is_ascii_alphabetic())
    }) {
        return Some(id.to_ascii_lowercase());
    }
    for caps in QUOTED_TITLE.captures_iter(typed) {
        let title = caps[1].trim();
        let grep = format!("--grep={title}");
        let Ok(out) = evidence.git_read(
            &[
                "log",
                "-i",
                "--fixed-strings",
                "-n",
                "6",
                "--format=%h %s",
                &grep,
            ],
            64 * 1024,
            deadline,
        ) else {
            continue;
        };
        let lines: Vec<&str> = out.lines().collect();
        let exact = lines.iter().find(|line| {
            line.split_once(' ')
                .is_some_and(|(_, subject)| subject.trim().eq_ignore_ascii_case(title))
        });
        if let Some(sha) = exact
            .or_else(|| lines.first())
            .and_then(|line| line.split_whitespace().next())
        {
            return Some(sha.to_owned());
        }
    }
    None
}

/// Whether the change to `path` is worth reading as code: not a lock file, a
/// generated or credential-shaped path, and not a deleted file.
fn reviewable(file: &ChangedFile) -> bool {
    file.status != 'D' && !region_pack::is_noise_path(&file.path)
}

/// The source files of `changes` no changed test file is about: a test file
/// is about a source file when its name carries the source file's stem.
pub(crate) fn unpaired_sources(changes: &Changes) -> Vec<String> {
    let stem = |path: &str| -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.split('.').next().unwrap_or(name).to_lowercase()
    };
    let tests: Vec<String> = changes
        .files
        .iter()
        .filter(|file| answer::is_test_path(&file.path))
        .map(|file| file.path.to_lowercase())
        .collect();
    changes
        .files
        .iter()
        .filter(|file| {
            file.status != 'D'
                && !answer::is_test_path(&file.path)
                && !answer::is_docs_path(&file.path)
                && !region_pack::is_noise_path(&file.path)
                && is_source_file(&file.path)
        })
        .filter(|file| {
            let stem = stem(&file.path);
            stem.len() < 2 || !tests.iter().any(|test| test.contains(&stem))
        })
        .map(|file| file.path.clone())
        .collect()
}

/// Whether `path` is a file of a programming language.
fn is_source_file(path: &str) -> bool {
    const SOURCE: &[&str] = &[
        "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "go", "java", "kt", "rb", "c", "h", "cc",
        "cpp", "hpp", "cs", "swift", "php", "scala", "ex", "exs", "lua", "sh",
    ];
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| SOURCE.contains(&extension))
}

/// Added tests named in the summary.
const TESTS_NAMED: usize = 12;
/// Files whose diff is shown, and lines of it.
const DIFF_FILES: usize = 4;
const DIFF_LINES: usize = 50;

/// The names of the tests the change adds: a `+` line that opens a test case
/// (`it("...")`, `test("...")`, `def test_x`, `fn x` after `#[test]`).
pub(crate) fn added_test_names(changes: &Changes) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for file in changes
        .files
        .iter()
        .filter(|file| answer::is_test_path(&file.path))
    {
        let added: Vec<String> = file
            .patch
            .iter()
            .filter_map(|line| line.strip_prefix('+'))
            .map(str::to_string)
            .collect();
        for index in 0..added.len() {
            if let Some(name) = answer::test_name(&added, index)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names
}

/// The lines of the summary block: the commit, the totals, each file with its
/// line counts, and what the tests say.
pub(crate) fn summary_lines(changes: &Changes) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(subject) = &changes.subject {
        lines.push(subject.clone());
    }
    let added: usize = changes.files.iter().map(|file| file.added).sum();
    let removed: usize = changes.files.iter().map(|file| file.removed).sum();
    lines.push(format!(
        "{} file(s) changed{}, +{added} -{removed}",
        changes.files.len(),
        if changes.truncated { " (list cut)" } else { "" }
    ));
    for file in changes.files.iter().take(SUMMARY_FILES) {
        lines.push(format!(
            "{} {} +{} -{}",
            file.status, file.path, file.added, file.removed
        ));
    }
    if changes.files.len() > SUMMARY_FILES {
        lines.push(format!(
            "… {} more files",
            changes.files.len() - SUMMARY_FILES
        ));
    }
    let tests = changes
        .files
        .iter()
        .filter(|file| answer::is_test_path(&file.path))
        .count();
    let added_tests = added_test_names(changes);
    if !added_tests.is_empty() {
        lines.push(format!(
            "tests added: {}{}",
            added_tests
                .iter()
                .take(TESTS_NAMED)
                .cloned()
                .collect::<Vec<_>>()
                .join("; "),
            if added_tests.len() > TESTS_NAMED {
                "; …"
            } else {
                ""
            }
        ));
    }
    let unpaired = unpaired_sources(changes);
    if unpaired.is_empty() {
        lines.push(format!("tests: {tests} of the changed files are tests; every changed source file has one beside it"));
    } else {
        let shown: Vec<&str> = unpaired
            .iter()
            .take(UNPAIRED_SHOWN)
            .map(String::as_str)
            .collect();
        lines.push(format!(
            "tests: {tests} of the changed files are tests; no changed test file is about {}{}",
            shown.join(", "),
            if unpaired.len() > UNPAIRED_SHOWN {
                ", …"
            } else {
                ""
            }
        ));
    }
    lines
}

/// The lines to read for the change `first..=last` of a file of `lines`: the
/// declaration that holds it, or the lines around it, at most
/// [`REGION_LINES`]; a change longer than that is shown from its start.
pub(crate) fn hunk_window(lines: &[String], first: u64, last: u64) -> Option<(u64, u64)> {
    let (top, _) = region_pack::match_region(lines, first)?;
    let (_, bottom) = region_pack::match_region(lines, last.max(first))?;
    let (top, bottom) = (top.min(first), bottom.max(last));
    let total = lines.len() as u64;
    if bottom.saturating_sub(top) < REGION_LINES {
        return Some((top, bottom.min(total)));
    }
    let start = first.saturating_sub(3).max(1);
    Some((start, (start + REGION_LINES - 1).min(total)))
}

/// The declarations among `skeleton` that a hunk of `hunks` touches.
pub(crate) fn touched<'a>(skeleton: &'a [SymbolHit], hunks: &[(u64, u64)]) -> Vec<&'a SymbolHit> {
    skeleton
        .iter()
        .filter(|hit| {
            matches!(
                hit.kind.as_str(),
                "function" | "method" | "class" | "struct" | "enum" | "trait" | "interface"
            )
        })
        .filter(|hit| {
            hunks
                .iter()
                .any(|&(first, last)| hit.start_line <= last && first <= hit.end_line)
        })
        .collect()
}

/// The blocks of a review: the summary, the changed code, the touched
/// declarations with their callers.
fn review_blocks(typed: &str, evidence: &dyn Evidence, deadline: Instant) -> Vec<Block> {
    let scope = named_commit(typed, evidence, deadline)
        .map_or_else(|| review_scope(typed), ChangeScope::Commit);
    let changes = [scope, ChangeScope::Latest]
        .into_iter()
        .find_map(|scope| evidence.changes(scope, deadline).ok());
    let Some(changes) = changes else {
        return Vec::new();
    };
    let mut blocks = vec![Block::new(
        format!("change summary — {}", changes.label),
        summary_lines(&changes),
    )];
    // What changed, as the diff has it: the removed lines are in no file.
    for file in changes
        .files
        .iter()
        .filter(|file| {
            reviewable(file) && !answer::is_test_path(&file.path) && !file.patch.is_empty()
        })
        .take(DIFF_FILES)
    {
        let mut patch: Vec<String> = file.patch.iter().take(DIFF_LINES).cloned().collect();
        if file.patch.len() > DIFF_LINES {
            patch.push("… diff cut".to_string());
        }
        blocks.push(Block::new(
            format!("diff of {} (+{} -{})", file.path, file.added, file.removed),
            patch,
        ));
    }
    let (mut regions, mut test_regions) = (0, 0);
    // The code under review first, its tests after, each by size of change.
    let mut reviewable_files: Vec<&ChangedFile> = changes
        .files
        .iter()
        .filter(|file| reviewable(file))
        .collect();
    reviewable_files.sort_by_key(|file| answer::is_test_path(&file.path));
    for file in reviewable_files.into_iter().take(REVIEW_FILES) {
        let is_test = answer::is_test_path(&file.path);
        if regions >= MAX_REGIONS
            || (is_test && test_regions >= MAX_TEST_REGIONS)
            || Instant::now() >= deadline
        {
            continue;
        }
        let Ok(lines) = evidence.lines_at(&file.path, 1, 20_000, deadline) else {
            continue;
        };
        let windows = locator::merge(
            file.hunks
                .iter()
                .filter_map(|&(first, last)| hunk_window(&lines, first, last))
                .collect(),
        );
        let per_file = if is_test {
            TEST_REGIONS_PER_FILE
        } else {
            REGIONS_PER_FILE
        };
        for (first, last) in windows.into_iter().take(per_file) {
            if regions >= MAX_REGIONS || (is_test && test_regions >= MAX_TEST_REGIONS) {
                break;
            }
            let label = format!(
                "changed in {} (+{} -{})",
                changes.label, file.added, file.removed
            );
            if let Some(block) = region_pack::range_block(label, &file.path, &lines, first, last) {
                blocks.push(block);
                regions += 1;
                test_regions += usize::from(is_test);
            }
        }
    }
    let mut notes: Vec<String> = Vec::new();
    let mut with_callers: Vec<SymbolHit> = Vec::new();
    for file in changes
        .files
        .iter()
        .filter(|file| reviewable(file) && !answer::is_test_path(&file.path))
        .take(TOUCHED_FILES)
    {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(skeleton) = evidence.skeleton(&file.path, deadline) else {
            continue;
        };
        for hit in touched(&skeleton, &file.hunks) {
            if notes.len() < MAX_TOUCHED {
                notes.push(format!(
                    "{}:{}-{} {} {}",
                    hit.path, hit.start_line, hit.end_line, hit.kind, hit.name
                ));
            }
            if matches!(hit.kind.as_str(), "function" | "method")
                && with_callers.len() < CALLER_SYMBOLS
            {
                with_callers.push(hit.clone());
            }
        }
    }
    for hit in &with_callers {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(callers) = evidence.callers(&hit.uid, deadline) else {
            continue;
        };
        let shown: Vec<String> = callers
            .iter()
            .filter(|caller| !answer::is_test_path(&caller.path))
            .take(CALLERS_EACH)
            .map(|caller| format!("{}:{} (in {})", caller.path, caller.line, caller.via))
            .collect();
        notes.push(if shown.is_empty() {
            format!("callers of {}: none found in the graph", hit.name)
        } else {
            format!("callers of {}: {}", hit.name, shown.join("; "))
        });
    }
    if !notes.is_empty() {
        blocks.push(Block::new(
            "touched declarations and their callers".to_string(),
            notes,
        ));
    }
    blocks
}

// ---------------------------------------------------------------------------
// The walkthrough: the calls out of the entry point, with their bodies
// ---------------------------------------------------------------------------

/// Calls followed out of an entry point: levels deep and nodes in all.
const TRACE_DEPTH: usize = 3;
const TRACE_NODES: usize = 10;
/// Entry points followed when the prompt names none.
const ROOTS: usize = 2;
/// Bodies shown.
const MAX_BODIES: usize = 8;
/// Meaning leads considered for an entry point.
const LEAD_CANDIDATES: usize = 6;

/// The listing of a trace: one line per call, indented by depth, in call
/// order.
pub(crate) fn trace_lines(nodes: &[FlowNode]) -> Vec<String> {
    nodes
        .iter()
        .map(|node| {
            format!(
                "{}{} {} — {}:{}-{}",
                "  ".repeat(node.depth),
                if node.depth == 0 { "entry" } else { "calls" },
                node.name,
                node.path,
                node.start,
                node.end
            )
        })
        .collect()
}

/// The declaration named `name`: the only one, or the one in `near`.
fn declaration(
    evidence: &dyn Evidence,
    name: &str,
    near: Option<&str>,
    deadline: Instant,
) -> Option<SymbolHit> {
    let hits = evidence.symbols(name, deadline).ok()?;
    let usable = |hit: &&SymbolHit| {
        !answer::is_test_path(&hit.path)
            && matches!(hit.kind.as_str(), "function" | "method" | "class")
    };
    match near.and_then(|path| hits.iter().find(|hit| hit.path == path && usable(hit))) {
        Some(hit) => Some(hit.clone()),
        None => {
            let mut usable: Vec<&SymbolHit> = hits.iter().filter(usable).collect();
            (usable.len() == 1).then(|| usable.remove(0).clone())
        }
    }
}

/// The nodes of a trace through the functions named in order (a call path the
/// graph found): no tree, each a step.
fn path_nodes(hops: &[String], evidence: &dyn Evidence, deadline: Instant) -> Vec<FlowNode> {
    let mut nodes: Vec<FlowNode> = Vec::new();
    for (depth, name) in hops.iter().enumerate() {
        if Instant::now() >= deadline {
            break;
        }
        if let Some(hit) = declaration(evidence, name, None, deadline) {
            nodes.push(FlowNode {
                depth,
                parent: depth.checked_sub(1).filter(|parent| *parent < nodes.len()),
                uid: hit.uid,
                name: hit.name,
                kind: hit.kind,
                path: hit.path,
                start: hit.start_line,
                end: hit.end_line,
            });
        }
    }
    nodes
}

/// Words of the request to be walked through something, which say nothing
/// about what.
const FLOW_NOISE: &[&str] = &[
    "walk",
    "through",
    "happen",
    "someone",
    "somebody",
    "onto",
    "until",
    "explain",
    "trace",
    "step",
    "flow",
    "lifecycle",
    "cycle",
    "show",
    "shows",
    "end",
    "whole",
    "entire",
    "exactly",
];
/// Declarations considered as the entry of a walkthrough.
const CANDIDATES: usize = 12;
/// Declarations asked of the graph for each word of the prompt.
const FIND_ROWS: usize = 80;
/// Nodes of the tree a candidate is scored on.
const SCORE_NODES: usize = 30;

/// A function scored as the entry of a walkthrough: the weight of the words
/// its own name carries (a rare word counts for more), plus half of what the
/// functions it reaches carry, plus half a point when the closest passage the
/// meaning search found is this one. The handler of a drop event carries
/// `drop` and reaches the image helpers; a helper carries `image` and
/// reaches nothing of the rest.
pub(crate) fn entry_score(own: f64, reached: f64, led: bool) -> f64 {
    own + 0.5 * reached + if led { 0.5 } else { 0.0 }
}

/// The weight the relevance probe gave `term`; one for a word it did not weigh.
fn weight(receipt: Option<&Receipt>, term: &str) -> f64 {
    receipt
        .and_then(|receipt| receipt.weight_of(term))
        .unwrap_or(1.0)
}

/// The entry points a trace starts from: the first symbol the prompt names;
/// else the function whose name carries the prompt's words and from which
/// most of the other such functions are reached (the handler, not the
/// helpers it calls); else the symbols of the closest passages the meaning
/// search found. Two at most.
fn roots(
    input: &KindInput,
    terms: &[String],
    evidence: &dyn Evidence,
    deadline: Instant,
) -> Vec<(SymbolHit, Vec<FlowNode>)> {
    if let Some(hit) = input
        .names
        .iter()
        .find_map(|name| declaration(evidence, name, None, deadline))
    {
        let tree = evidence
            .flow_tree(&hit.uid, TRACE_DEPTH, TRACE_NODES, deadline)
            .unwrap_or_default();
        return vec![(hit, tree)];
    }
    // One search for each word: the rare word's functions must not be
    // crowded out of the rows by the common word's.
    let mut found: Vec<SymbolHit> = Vec::new();
    for term in terms {
        let rows = evidence
            .find_symbols(std::slice::from_ref(term), FIND_ROWS, deadline)
            .unwrap_or_default();
        for hit in rows {
            if !answer::is_test_path(&hit.path)
                && matches!(hit.kind.as_str(), "function" | "method")
                && locator::carries_term(&hit.name, term)
                && !found.iter().any(|held| held.uid == hit.uid)
            {
                found.push(hit);
            }
        }
    }
    let carried_weight = |name: &str| -> f64 {
        terms
            .iter()
            .filter(|term| locator::carries_term(name, term))
            .map(|term| weight(input.receipt, term))
            .sum()
    };
    found.sort_by(|a, b| {
        carried_weight(&b.name)
            .total_cmp(&carried_weight(&a.name))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    found.truncate(CANDIDATES);
    let led = |hit: &SymbolHit| {
        input
            .leads
            .iter()
            .any(|(symbol, path)| *symbol == hit.name && *path == hit.path)
    };
    let mut scored: Vec<(f64, SymbolHit, Vec<FlowNode>)> = Vec::new();
    for hit in &found {
        if Instant::now() >= deadline {
            break;
        }
        let tree = evidence
            .flow_tree(&hit.uid, TRACE_DEPTH, SCORE_NODES, deadline)
            .unwrap_or_default();
        let reached: f64 = tree
            .iter()
            .skip(1)
            .filter(|node| found.iter().any(|other| other.uid == node.uid))
            .map(|node| carried_weight(&node.name))
            .sum();
        scored.push((
            entry_score(carried_weight(&hit.name), reached, led(hit)),
            hit.clone(),
            tree,
        ));
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
    });
    let mut chosen: Vec<(SymbolHit, Vec<FlowNode>)> = Vec::new();
    for (_, hit, mut tree) in scored {
        // A second entry that the first already reaches adds nothing.
        let reached_already = chosen
            .iter()
            .any(|(_, held)| held.iter().any(|node| node.uid == hit.uid));
        if chosen.len() < ROOTS && !reached_already {
            tree.truncate(TRACE_NODES);
            chosen.push((hit, tree));
        }
    }
    if !chosen.is_empty() {
        return chosen;
    }
    for (symbol, path) in input.leads.iter().take(LEAD_CANDIDATES) {
        if chosen.len() >= ROOTS || Instant::now() >= deadline {
            break;
        }
        if answer::is_test_path(path) || chosen.iter().any(|(hit, _)| hit.name == *symbol) {
            continue;
        }
        if let Some(hit) = declaration(evidence, symbol, Some(path), deadline) {
            let tree = evidence
                .flow_tree(&hit.uid, TRACE_DEPTH, TRACE_NODES, deadline)
                .unwrap_or_default();
            chosen.push((hit, tree));
        }
    }
    chosen
}

/// The blocks of a walkthrough: the trace, then the body of each call.
fn flow_blocks(input: &KindInput, evidence: &dyn Evidence, deadline: Instant) -> Vec<Block> {
    let mut nodes: Vec<FlowNode> = Vec::new();
    if input.hops.len() >= 2 {
        nodes = path_nodes(input.hops, evidence, deadline);
    }
    if nodes.len() < 2 {
        nodes.clear();
        let candidates: Vec<String> = locator::terms(input.typed)
            .into_iter()
            .filter(|term| !FLOW_NOISE.contains(&term.as_str()))
            .collect();
        let terms = locator::pick_terms(&candidates, input.receipt);
        for (_, tree) in roots(input, &terms, evidence, deadline) {
            let offset = nodes.len();
            nodes.extend(tree.into_iter().map(|mut node| {
                node.parent = node.parent.map(|parent| parent + offset);
                node
            }));
        }
    }
    if nodes.len() < 2 {
        return Vec::new();
    }
    let mut blocks = vec![Block::new(
        "call trace, in call order".to_string(),
        trace_lines(&nodes),
    )];
    let mut shown: Vec<(String, u64, u64)> = Vec::new();
    for node in nodes.iter().take(MAX_BODIES + 4) {
        if shown.len() >= MAX_BODIES || Instant::now() >= deadline {
            break;
        }
        if shown.iter().any(|(path, first, last)| {
            *path == node.path && *first <= node.end && node.start <= *last
        }) {
            continue;
        }
        let Ok(lines) = evidence.lines_at(&node.path, 1, 20_000, deadline) else {
            continue;
        };
        let site = DefSite {
            path: node.path.clone(),
            start: node.start,
            end: Some(node.end),
            kind: node.kind.clone(),
        };
        if let Some(block) = region_pack::def_block(&lines, &site, &node.name) {
            shown.push((node.path.clone(), node.start, node.end));
            blocks.push(block);
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// History: which commit, when, and what it touched
// ---------------------------------------------------------------------------

/// Commits listed at most, and paths shown under each.
const MAX_COMMITS: usize = 8;
const PATHS_PER_COMMIT: usize = 3;
/// Commits whose changes are read (one git call each), and files shown.
const DETAILED_COMMITS: usize = 2;
const CHANGED_FILES: usize = 6;
/// Rows each history search asks for.
const HISTORY_ROWS: usize = 40;
/// Words of the prompt searched together in the first search.
const HISTORY_WORDS: usize = 3;
/// Shortest changed line worth looking for in the current file.
const MIN_LINE_CHARS: usize = 8;
/// Words that ask about history and name nothing to search for.
const HISTORY_NOISE: &[&str] = &[
    "when",
    "commit",
    "commits",
    "change",
    "changed",
    "changes",
    "start",
    "started",
    "added",
    "introduced",
    "introduce",
    "removed",
    "since",
    "which",
    "history",
    "first",
    "land",
    "landed",
    "appeared",
    "version",
    "release",
    "released",
    "happen",
    "happened",
    "exactly",
    "file",
    "files",
    "code",
    "did",
    "does",
    "was",
];

/// One commit of a history answer, with where it touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Commit {
    pub(crate) sha: String,
    pub(crate) date: String,
    pub(crate) subject: String,
    /// `(path, a changed line)` for each path the search matched.
    pub(crate) touched: Vec<(String, Option<String>)>,
    /// What the commit changed, one `path:line +a -r` per file, when read.
    pub(crate) changed: Vec<String>,
    /// How many hits (of any facet, of any search) named the commit.
    pub(crate) hits: usize,
}

/// The hits grouped by commit, the commit most hits name first and the most
/// recent among equals, at most [`MAX_COMMITS`].
pub(crate) fn group_commits(hits: &[HistoryHit]) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();
    for hit in hits {
        let index = match commits.iter().position(|commit| commit.sha == hit.sha) {
            Some(index) => index,
            None => {
                commits.push(Commit {
                    sha: hit.sha.clone(),
                    date: hit.at.chars().take(10).collect(),
                    subject: hit.subject.clone(),
                    touched: Vec::new(),
                    changed: Vec::new(),
                    hits: 0,
                });
                commits.len() - 1
            }
        };
        let commit = &mut commits[index];
        commit.hits += 1;
        if commit.date.is_empty() {
            commit.date = hit.at.chars().take(10).collect();
        }
        if let Some(path) = &hit.path {
            match commit.touched.iter_mut().find(|(held, _)| held == path) {
                Some((_, line)) if line.is_none() => *line = hit.snippet.clone(),
                Some(_) => {}
                None => commit.touched.push((path.clone(), hit.snippet.clone())),
            }
        }
    }
    commits.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| b.date.cmp(&a.date)));
    commits.truncate(MAX_COMMITS);
    commits
}

/// The 1-based line of `lines` that holds a line of `snippet` (an added or a
/// removed line of a diff, with its `+`/`-` marker), the first that is long
/// enough to mean something.
pub(crate) fn locate_changed_line(lines: &[String], snippet: &str) -> Option<u64> {
    snippet
        .lines()
        .map(|line| line.trim_start_matches(['+', '-', ' ']).trim())
        .filter(|line| line.chars().count() >= MIN_LINE_CHARS)
        .find_map(|wanted| {
            lines
                .iter()
                .position(|line| line.trim() == wanted)
                .map(|at| at as u64 + 1)
        })
}

/// The lines of the history block: `sha date subject`, then each touched path
/// as `path:line` (the line of the current file that holds a line the commit
/// changed) or bare when no such line is found.
pub(crate) fn history_lines(
    commits: &[Commit],
    line_of: &dyn Fn(&str, &str) -> Option<u64>,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for commit in commits {
        let date = if commit.date.is_empty() {
            String::new()
        } else {
            format!(" {}", commit.date)
        };
        lines.push(format!("{}{date} {}", commit.sha, commit.subject));
        lines.extend(
            commit
                .changed
                .iter()
                .map(|change| format!("  changed {change}")),
        );
        for (path, snippet) in commit.touched.iter().take(PATHS_PER_COMMIT) {
            let place = snippet
                .as_deref()
                .and_then(|snippet| line_of(path, snippet))
                .map_or_else(|| path.clone(), |line| format!("{path}:{line}"));
            lines.push(format!("  touched {place}"));
        }
    }
    lines
}

/// The raw content words of the prompt a history search can use, rarest first
/// by the probe's weight; a word the probe found everywhere is left out.
pub(crate) fn history_words(typed: &str, receipt: Option<&Receipt>) -> Vec<String> {
    let mut words: Vec<(String, f64)> = Vec::new();
    for word in crate::prompt::brief_keywords(crate::prompt::brief_task(typed)) {
        let word = word.to_lowercase();
        let stem = locator::stem(&word);
        if word.len() < 4
            || HISTORY_NOISE.contains(&word.as_str())
            || locator::is_generic(&word)
            || words.iter().any(|(held, _)| *held == word)
        {
            continue;
        }
        match receipt.and_then(|receipt| receipt.weight_of(&stem)) {
            Some(weight) if weight <= 0.0 => {}
            weight => words.push((word, weight.unwrap_or(1.0))),
        }
    }
    words.sort_by(|a, b| b.1.total_cmp(&a.1));
    words.into_iter().map(|(word, _)| word).collect()
}

/// The history block: the commits that match the prompt's words, as the
/// history index has them.
fn history_blocks(input: &KindInput, evidence: &dyn Evidence, deadline: Instant) -> Vec<Block> {
    let words = history_words(input.typed, input.receipt);
    if words.is_empty() {
        return Vec::new();
    }
    let mut queries = vec![
        words
            .iter()
            .take(HISTORY_WORDS)
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    ];
    if words.len() > 1 {
        queries.push(words[0].clone());
    }
    let mut hits: Vec<HistoryHit> = Vec::new();
    for query in queries {
        if Instant::now() >= deadline {
            break;
        }
        if let Ok((found, _)) = evidence.history(&query, HISTORY_ROWS, deadline) {
            hits.extend(found);
        }
    }
    let mut commits = group_commits(&hits);
    if commits.is_empty() {
        return Vec::new();
    }
    // What the best commits changed, at the lines of the version they made.
    for commit in commits.iter_mut().take(DETAILED_COMMITS) {
        if Instant::now() >= deadline {
            break;
        }
        if let Ok(change) = evidence.changes(ChangeScope::Commit(commit.sha.clone()), deadline) {
            commit.changed = change
                .files
                .iter()
                .take(CHANGED_FILES)
                .map(|file| {
                    let line = file
                        .hunks
                        .first()
                        .map_or(String::new(), |(first, _)| format!(":{first}"));
                    format!("{}{line} +{} -{}", file.path, file.added, file.removed)
                })
                .collect();
        }
    }
    let cache: std::cell::RefCell<BTreeMap<String, Option<Vec<String>>>> =
        std::cell::RefCell::default();
    let lines = history_lines(&commits, &|path, snippet| {
        let mut held = cache.borrow_mut();
        let source = held
            .entry(path.to_string())
            .or_insert_with(|| evidence.lines_at(path, 1, 20_000, deadline).ok());
        source
            .as_ref()
            .and_then(|lines| locate_changed_line(lines, snippet))
    });
    vec![Block::new(
        format!(
            "history — commits matching {}",
            words
                .iter()
                .take(HISTORY_WORDS)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ),
        lines,
    )]
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// What a kind's pack is made from.
pub(crate) struct KindInput<'a> {
    pub(crate) kind: QuestionKind,
    pub(crate) typed: &'a str,
    /// Identifiers the prompt names.
    pub(crate) names: &'a [String],
    /// `(symbol, path)` of the passages the meaning search found, best first.
    pub(crate) leads: &'a [(String, String)],
    /// The functions of a call path the chain already found, in order.
    pub(crate) hops: &'a [String],
    /// What the relevance probe weighed in the prompt's words.
    pub(crate) receipt: Option<&'a Receipt>,
}

/// The blocks of the pack of `input.kind`, best first; empty for a kind that
/// has none or when nothing could be read.
pub(crate) fn blocks(input: &KindInput, evidence: &dyn Evidence, deadline: Instant) -> Vec<Block> {
    match input.kind {
        QuestionKind::Overview => overview_blocks(evidence, deadline),
        QuestionKind::Review => review_blocks(input.typed, evidence, deadline),
        QuestionKind::History => history_blocks(input, evidence, deadline),
        QuestionKind::Flow => flow_blocks(input, evidence, deadline),
        _ => Vec::new(),
    }
}

/// Whether the pack of `kind` replaces the regions closest to the question:
/// those would be the files of a few words, not the repository or the change.
pub(crate) const fn owns_the_pack(kind: QuestionKind) -> bool {
    matches!(
        kind,
        QuestionKind::Overview | QuestionKind::Review | QuestionKind::History
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{CallerHit, Found};
    use std::time::Duration;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    fn file(path: &str, lang: &str, symbols: usize) -> MapFile {
        MapFile {
            path: path.to_string(),
            lang: lang.to_string(),
            symbols,
        }
    }

    fn top(path: &str, name: &str, kind: &str, line: u64, end: u64, sig: &str) -> TopSymbol {
        TopSymbol {
            path: path.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            end,
            sig: sig.to_string(),
        }
    }

    // ---- areas and faces -----------------------------------------------------

    #[test]
    fn split_areas_should_keep_a_small_repository_whole_and_split_a_large_one_by_directory() {
        let small: Vec<MapFile> = (0..5)
            .map(|n| file(&format!("src/f{n}.py"), "python", 1))
            .collect();
        let areas = split_areas(&small);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].prefix, "");
        // Past SPLIT_FILES the top directories become areas; one that is itself
        // large is split again, and files at its root stay with it.
        let mut big: Vec<MapFile> = Vec::new();
        for n in 0..70 {
            big.push(file(&format!("crates/core/src/m{n}.rs"), "rust", 2));
        }
        for n in 0..3 {
            big.push(file(&format!("crates/cli/src/m{n}.rs"), "rust", 2));
        }
        big.push(file("crates/core/lib.rs", "rust", 1));
        big.push(file("build.rs", "rust", 1));
        let areas = split_areas(&big);
        let prefixes: Vec<&str> = areas.iter().map(|area| area.prefix.as_str()).collect();
        assert_eq!(
            prefixes,
            ["", "crates/cli", "crates/core", "crates/core/src"]
        );
        let sizes: Vec<usize> = areas.iter().map(|area| area.files.len()).collect();
        assert_eq!(sizes, [1, 3, 1, 70]);
        // Every file is in exactly one area.
        assert_eq!(sizes.iter().sum::<usize>(), big.len());
    }

    #[test]
    fn faces_should_prefer_public_types_then_functions_and_drop_tests() {
        let items = [
            top("src/a.rs", "helper", "function", 40, 44, "fn helper("),
            top("src/a.rs", "Store", "struct", 10, 90, "pub struct Store"),
            top("src/a.rs", "open", "function", 100, 130, "pub fn open("),
            top("src/a.rs", "Row", "struct", 5, 8, "struct Row"),
            top(
                "tests/t.rs",
                "Fixture",
                "struct",
                1,
                99,
                "pub struct Fixture",
            ),
        ];
        let refs: Vec<&TopSymbol> = items.iter().collect();
        let names: Vec<&str> = faces(&refs, 3)
            .iter()
            .map(|top| top.name.as_str())
            .collect();
        // The three best, in file order; the test file never counts.
        assert_eq!(names, ["Row", "Store", "open"]);
        assert_eq!(faces(&refs, 0).len(), 0);
    }

    #[test]
    fn map_lines_should_list_a_small_repository_file_by_file_with_path_and_line() {
        let map = RepoMap {
            files: vec![
                file("src/app.py", "python", 5),
                file("src/util.py", "python", 0),
                file("tests/test_app.py", "python", 9),
                file("docs/conf.py", "python", 1),
            ],
            symbols: 15,
            tops: vec![
                top("src/app.py", "Session", "class", 10, 80, "class Session:"),
                top("src/app.py", "merge", "function", 90, 99, "def merge("),
            ],
            mains: vec![top("src/app.py", "main", "function", 120, 130, "def main(")],
        };
        let lines = map_lines(&map);
        assert_eq!(lines[0], "4 indexed files, 15 symbols (python 4)");
        assert_eq!(
            lines[1],
            "src/app.py (5 symbols): src/app.py:10 class Session; src/app.py:90 fn merge"
        );
        assert_eq!(lines[2], "no declarations of their own: src/util.py");
        // Tests and documentation are one line, with where they are.
        assert_eq!(
            lines[3],
            "tests, docs, scripts and the like: docs/ (1), tests/ (1)"
        );
        assert_eq!(lines[4], "entry points: src/app.py:120 fn main");
    }

    #[test]
    fn map_lines_should_list_a_large_repository_by_area_and_cut_the_tail() {
        let mut files: Vec<MapFile> = Vec::new();
        let mut tops: Vec<TopSymbol> = Vec::new();
        for crate_ in 0..(MAX_AREAS + 3) {
            for n in 0..3 {
                let path = format!("crates/c{crate_:02}/src/m{n}.rs");
                files.push(file(&path, "rust", 2));
                if n == 0 {
                    tops.push(top(
                        &path,
                        &format!("Type{crate_}"),
                        "struct",
                        7,
                        20,
                        "pub struct T",
                    ));
                }
            }
        }
        for n in 0..(SMALL_REPO_FILES) {
            files.push(file(&format!("crates/big/src/m{n}.rs"), "rust", 1));
        }
        let map = RepoMap {
            symbols: files.iter().map(|file| file.symbols).sum(),
            files,
            tops,
            mains: Vec::new(),
        };
        let lines = map_lines(&map);
        let areas: Vec<&String> = lines.iter().filter(|line| line.contains(" — ")).collect();
        assert_eq!(areas.len(), MAX_AREAS);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("… 4 more areas (")),
            "{lines:?}"
        );
        let first = areas
            .iter()
            .find(|line| line.starts_with("crates/c00/"))
            .unwrap();
        assert_eq!(
            first.as_str(),
            "crates/c00/ — 3 files, 6 symbols, rust: crates/c00/src/m0.rs:7 struct Type0"
        );
    }

    #[test]
    fn is_primary_should_drop_tests_hidden_and_auxiliary_directories() {
        for path in ["src/lib.rs", "crates/a/src/x.rs", "app/main.py"] {
            assert!(is_primary(path), "{path}");
        }
        for path in [
            "tests/a.rs",
            "src/a_test.go",
            ".github/x.py",
            "docs/conf.py",
            "scripts/build.py",
            "crates/b/benches/x.rs",
            "eval/run.py",
        ] {
            assert!(!is_primary(path), "{path}");
        }
    }

    #[test]
    fn outline_should_list_three_levels_of_headings_and_skip_fenced_code() {
        let doc = strings(&[
            "# Title",
            "text",
            "## Crates",
            "```sh",
            "# not a heading",
            "```",
            "### Detail",
            "#### too deep",
            "#hashtag",
            "## Wire",
        ]);
        assert_eq!(
            outline("ARCHITECTURE.md", &doc),
            [
                "ARCHITECTURE.md:1 # Title",
                "ARCHITECTURE.md:3 ## Crates",
                "ARCHITECTURE.md:7 ### Detail",
                "ARCHITECTURE.md:10 ## Wire"
            ]
        );
    }

    // ---- review ------------------------------------------------------------

    #[test]
    fn review_scope_should_follow_the_words_and_default_to_the_latest_commit() {
        for (prompt, scope) in [
            ("review the latest commit", ChangeScope::Latest),
            ("is the last commit risky", ChangeScope::Latest),
            ("review this PR for regressions", ChangeScope::Branch),
            (
                "check my branch before I open a pull request",
                ChangeScope::Branch,
            ),
            ("review my changes", ChangeScope::Working),
            (
                "anything risky in the uncommitted diff",
                ChangeScope::Working,
            ),
            ("look over what I changed", ChangeScope::Working),
            ("review it", ChangeScope::Latest),
        ] {
            assert_eq!(review_scope(prompt), scope, "{prompt}");
        }
    }

    fn changed(
        path: &str,
        status: char,
        added: usize,
        removed: usize,
        hunks: &[(u64, u64)],
    ) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            status,
            added,
            removed,
            hunks: hunks.to_vec(),
            patch: Vec::new(),
        }
    }

    fn change(files: Vec<ChangedFile>) -> Changes {
        Changes {
            label: "commit abc1234".to_string(),
            subject: Some("abc1234 fix the frame clip".to_string()),
            files,
            truncated: false,
        }
    }

    #[test]
    fn unpaired_sources_should_name_the_source_files_no_changed_test_is_about() {
        let changes = change(vec![
            changed("src/frame.ts", 'M', 5, 1, &[(1, 5)]),
            changed("src/bounds.ts", 'M', 2, 0, &[(9, 10)]),
            changed("src/deleted.ts", 'D', 0, 9, &[]),
            changed("tests/frame.test.ts", 'M', 8, 0, &[(1, 8)]),
            changed("README.md", 'M', 1, 0, &[(1, 1)]),
            changed("Cargo.lock", 'M', 4, 4, &[(1, 4)]),
            changed("src/x.rs", 'M', 1, 0, &[(1, 1)]),
        ]);
        // `frame.ts` has `frame.test.ts`; `x` is one letter, too short to pair.
        assert_eq!(unpaired_sources(&changes), ["src/bounds.ts", "src/x.rs"]);
    }

    #[test]
    fn summary_lines_should_carry_the_totals_each_file_and_the_test_fact() {
        let changes = change(vec![
            changed("src/frame.ts", 'M', 5, 1, &[(1, 5)]),
            changed("tests/frame.test.ts", 'A', 8, 0, &[(1, 8)]),
        ]);
        let lines = summary_lines(&changes);
        assert_eq!(lines[0], "abc1234 fix the frame clip");
        assert_eq!(lines[1], "2 file(s) changed, +13 -1");
        assert_eq!(lines[2], "M src/frame.ts +5 -1");
        assert_eq!(lines[3], "A tests/frame.test.ts +8 -0");
        assert_eq!(
            lines[4],
            "tests: 1 of the changed files are tests; every changed source file has one beside it"
        );
        let untested = change(vec![changed("src/frame.ts", 'M', 5, 1, &[(1, 5)])]);
        let text = summary_lines(&untested).join("\n");
        assert!(
            text.contains("tests: 0 of the changed files are tests; no changed test file is about src/frame.ts"),
            "{text}"
        );
    }

    #[test]
    fn summary_lines_should_cut_a_long_list_and_say_when_the_list_was_cut() {
        let files: Vec<ChangedFile> = (0..SUMMARY_FILES + 5)
            .map(|n| changed(&format!("src/f{n}.rs"), 'M', 1, 1, &[(1, 1)]))
            .collect();
        let mut changes = change(files);
        changes.truncated = true;
        let lines = summary_lines(&changes);
        assert!(lines[1].contains("(list cut)"), "{}", lines[1]);
        assert!(lines.contains(&"… 5 more files".to_string()));
    }

    #[test]
    fn hunk_window_should_take_the_function_around_a_change_and_cap_a_long_one() {
        let mut source = strings(&[
            "use x;",
            "",
            "/// Doc.",
            "pub fn small(a: u8) -> u8 {",
            "    let b = a + 1;",
            "    b",
            "}",
            "",
            "pub fn other() {}",
        ]);
        // A change inside `small` shows `small` with its doc comment.
        assert_eq!(hunk_window(&source, 5, 5), Some((3, 7)));
        // A change across two functions shows both.
        assert_eq!(hunk_window(&source, 5, 9), Some((3, 9)));
        // A line past the end has no window.
        assert_eq!(hunk_window(&source, 99, 99), None);
        // A long change is shown from its start, at most REGION_LINES.
        source.extend((0..200).map(|n| format!("    // line {n}")));
        let (first, last) = hunk_window(&source, 20, 150).unwrap();
        assert_eq!(first, 17);
        assert_eq!(last - first + 1, REGION_LINES);
    }

    fn hit(path: &str, name: &str, kind: &str, start: u64, end: u64) -> SymbolHit {
        SymbolHit {
            uid: format!("{path}#{name}#{kind}"),
            name: name.to_string(),
            kind: kind.to_string(),
            path: path.to_string(),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn touched_should_keep_the_declarations_a_hunk_overlaps() {
        let skeleton = vec![
            hit("a.rs", "one", "function", 1, 10),
            hit("a.rs", "two", "function", 12, 20),
            hit("a.rs", "LIMIT", "const", 22, 22),
            hit("a.rs", "Three", "struct", 30, 40),
        ];
        let names = |hunks: &[(u64, u64)]| -> Vec<String> {
            touched(&skeleton, hunks)
                .iter()
                .map(|hit| hit.name.clone())
                .collect()
        };
        assert_eq!(names(&[(5, 5)]), ["one"]);
        assert_eq!(names(&[(10, 12)]), ["one", "two"]);
        assert_eq!(
            names(&[(22, 22)]),
            Vec::<String>::new(),
            "a constant is not a touched declaration"
        );
        assert_eq!(names(&[(35, 35), (1, 1)]), ["one", "Three"]);
        assert!(names(&[]).is_empty());
    }

    // ---- the walkthrough ---------------------------------------------------

    fn node(
        depth: usize,
        parent: Option<usize>,
        name: &str,
        path: &str,
        start: u64,
        end: u64,
    ) -> FlowNode {
        FlowNode {
            depth,
            parent,
            uid: format!("{path}#{name}#function"),
            name: name.to_string(),
            kind: "function".to_string(),
            path: path.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn trace_lines_should_indent_each_call_under_its_caller() {
        let nodes = vec![
            node(0, None, "on_drop", "app.ts", 10, 40),
            node(1, Some(0), "insert", "img.ts", 5, 30),
            node(2, Some(1), "load", "img.ts", 40, 50),
            node(1, Some(0), "render", "view.ts", 1, 9),
        ];
        assert_eq!(
            trace_lines(&nodes),
            [
                "entry on_drop — app.ts:10-40",
                "  calls insert — img.ts:5-30",
                "    calls load — img.ts:40-50",
                "  calls render — view.ts:1-9"
            ]
        );
    }

    #[test]
    fn entry_score_should_weigh_the_rare_word_and_what_the_function_reaches() {
        // A function whose name carries the rare word beats one that carries
        // two common ones; reaching the others adds half of their weight.
        assert!(entry_score(4.0, 0.0, false) > entry_score(1.5 + 1.5, 0.0, false));
        assert!(entry_score(4.0, 3.0, false) > entry_score(4.0, 0.0, false));
        assert!((entry_score(2.0, 2.0, true) - 3.5).abs() < 1e-9);
    }

    // ---- gathering, against a fake repository ---------------------------------

    #[derive(Default)]
    struct World {
        map: Option<RepoMap>,
        changes: Option<Changes>,
        files: Vec<(String, Vec<String>)>,
        skeletons: Vec<(String, Vec<SymbolHit>)>,
        symbols: Vec<SymbolHit>,
        callers: Vec<(String, Vec<CallerHit>)>,
        trees: Vec<(String, Vec<FlowNode>)>,
        like: Vec<SymbolHit>,
        history: Vec<HistoryHit>,
        /// What `git log` prints for any title: `<sha> <subject>` lines.
        git_log: Option<String>,
    }

    impl World {
        fn with_file(mut self, path: &str, lines: &[&str]) -> Self {
            self.files.push((path.to_string(), strings(lines)));
            self
        }
    }

    impl Evidence for World {
        fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("unsupported".into())
        }
        fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("unsupported".into())
        }
        fn symbols(&self, name: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            Ok(self
                .symbols
                .iter()
                .filter(|hit| hit.name == name)
                .cloned()
                .collect())
        }
        fn callers(&self, target: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            self.callers
                .iter()
                .find(|(uid, _)| uid == target)
                .map(|(_, callers)| callers.clone())
                .ok_or_else(|| "no callers".to_string())
        }
        fn skeleton(&self, file: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            self.skeletons
                .iter()
                .find(|(path, _)| path == file)
                .map(|(_, hits)| hits.clone())
                .ok_or_else(|| "no skeleton".to_string())
        }
        fn repo_map(&self, _: Instant) -> Result<RepoMap, String> {
            self.map.clone().ok_or_else(|| "no map".to_string())
        }
        fn changes(&self, scope: ChangeScope, _: Instant) -> Result<Changes, String> {
            self.changes
                .clone()
                .filter(|_| matches!(scope, ChangeScope::Latest | ChangeScope::Commit(_)))
                .ok_or_else(|| "no changes".to_string())
        }
        fn git_read(&self, args: &[&str], _: usize, _: Instant) -> Result<String, String> {
            assert_eq!(args[0], "log", "{args:?}");
            self.git_log.clone().ok_or_else(|| "no git".to_string())
        }
        fn history(
            &self,
            phrase: &str,
            _: usize,
            _: Instant,
        ) -> Result<(Vec<HistoryHit>, Vec<String>), String> {
            if self.history.is_empty() {
                return Err("facts index absent".to_string());
            }
            let words: Vec<&str> = phrase.split_whitespace().collect();
            let found = self
                .history
                .iter()
                .filter(|hit| words.iter().all(|word| hit.subject.contains(word)))
                .cloned()
                .collect();
            Ok((found, Vec::new()))
        }
        fn find_symbols(
            &self,
            terms: &[String],
            _: usize,
            _: Instant,
        ) -> Result<Vec<SymbolHit>, String> {
            Ok(self
                .like
                .iter()
                .filter(|hit| {
                    terms
                        .iter()
                        .any(|term| locator::carries_term(&hit.name, term))
                })
                .cloned()
                .collect())
        }
        fn flow_tree(
            &self,
            uid: &str,
            _: usize,
            limit: usize,
            _: Instant,
        ) -> Result<Vec<FlowNode>, String> {
            self.trees
                .iter()
                .find(|(root, _)| root == uid)
                .map(|(_, tree)| tree.iter().take(limit).cloned().collect())
                .ok_or_else(|| "no tree".to_string())
        }
        fn lines_at(
            &self,
            path: &str,
            start: u64,
            end: u64,
            _: Instant,
        ) -> Result<Vec<String>, String> {
            let (_, lines) = self
                .files
                .iter()
                .find(|(held, _)| held == path)
                .ok_or_else(|| "no such file".to_string())?;
            Ok(lines
                .iter()
                .skip(usize::try_from(start.saturating_sub(1)).unwrap_or(0))
                .take(usize::try_from(end + 1 - start).unwrap_or(0))
                .cloned()
                .collect())
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn input<'a>(
        kind: QuestionKind,
        typed: &'a str,
        names: &'a [String],
        leads: &'a [(String, String)],
        hops: &'a [String],
    ) -> KindInput<'a> {
        KindInput {
            kind,
            typed,
            names,
            leads,
            hops,
            receipt: None,
        }
    }

    fn heads(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .map(|block| block.text().lines().next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn an_overview_should_carry_the_map_the_architecture_head_its_outline_the_manifest_and_the_readme()
     {
        let architecture: Vec<String> = (1..=100)
            .map(|n| {
                if n % 30 == 1 {
                    format!("## Part {n}")
                } else {
                    format!("line {n}")
                }
            })
            .collect();
        let mut world = World {
            map: Some(RepoMap {
                files: vec![file("src/a.rs", "rust", 1)],
                symbols: 1,
                tops: vec![top("src/a.rs", "Store", "struct", 3, 9, "pub struct Store")],
                mains: Vec::new(),
            }),
            ..World::default()
        };
        world
            .files
            .push(("ARCHITECTURE.md".to_string(), architecture));
        let world = world
            .with_file("Cargo.toml", &["[workspace]", "members = [\"a\"]"])
            .with_file("README.md", &["# Hello", "text"]);
        let blocks = blocks(
            &input(QuestionKind::Overview, "give me a tour", &[], &[], &[]),
            &world,
            soon(),
        );
        assert_eq!(
            heads(&blocks),
            [
                "### repo map",
                "### ARCHITECTURE.md:1-70 — architecture document",
                "### architecture document outline",
                "### Cargo.toml:1-2 — project manifest",
                "### README.md:1-2 — readme"
            ]
        );
        assert!(blocks[0].text().contains("src/a.rs:3 struct Store"));
        assert!(blocks[1].text().contains("70| line 70"));
        assert!(!blocks[1].text().contains("71|"));
        assert!(blocks[2].text().contains("ARCHITECTURE.md:31 ## Part 31"));
    }

    #[test]
    fn an_overview_should_read_the_readme_longer_when_no_architecture_document_exists() {
        let readme: Vec<String> = (1..=100).map(|n| format!("readme {n}")).collect();
        let mut world = World::default();
        world.files.push(("README.md".to_string(), readme));
        let blocks = blocks(
            &input(QuestionKind::Overview, "tour of the repo", &[], &[], &[]),
            &world,
            soon(),
        );
        assert_eq!(heads(&blocks), ["### README.md:1-70 — readme"]);
        // Nothing to read, nothing to say.
        assert!(
            super::blocks(
                &input(QuestionKind::Overview, "tour of the repo", &[], &[], &[]),
                &World::default(),
                soon()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_review_should_carry_the_summary_the_changed_code_and_the_callers_of_what_it_touched() {
        let source: Vec<&str> = vec![
            "use x;",
            "",
            "pub fn clip(a: u8) -> u8 {",
            "    let b = a + 1;",
            "    b",
            "}",
            "",
            "pub fn other() {}",
        ];
        let touched_hit = hit("src/frame.rs", "clip", "function", 3, 6);
        let mut world = World {
            changes: Some(change(vec![
                changed("src/frame.rs", 'M', 1, 1, &[(4, 4)]),
                changed("tests/frame_test.rs", 'M', 3, 0, &[(1, 3)]),
                changed("Cargo.lock", 'M', 2, 2, &[(1, 2)]),
                changed("src/gone.rs", 'D', 0, 5, &[(1, 1)]),
            ])),
            skeletons: vec![(
                "src/frame.rs".to_string(),
                vec![
                    touched_hit.clone(),
                    hit("src/frame.rs", "other", "function", 8, 8),
                ],
            )],
            callers: vec![(
                touched_hit.uid.clone(),
                vec![
                    CallerHit {
                        path: "src/main.rs".into(),
                        via: "run".into(),
                        line: 12,
                    },
                    CallerHit {
                        path: "tests/frame_test.rs".into(),
                        via: "t".into(),
                        line: 2,
                    },
                ],
            )],
            ..World::default()
        }
        .with_file("src/frame.rs", &source)
        .with_file("tests/frame_test.rs", &["#[test]", "fn t() {}", "// x"]);
        world
            .files
            .push(("Cargo.lock".to_string(), strings(&["a", "b"])));
        let blocks = blocks(
            &input(
                QuestionKind::Review,
                "review the latest commit",
                &[],
                &[],
                &[],
            ),
            &world,
            soon(),
        );
        let heads = heads(&blocks);
        assert_eq!(heads[0], "### change summary — commit abc1234");
        // The code under review first, then its test; no lock file, no deletion.
        assert_eq!(
            heads[1],
            "### src/frame.rs:3-6 — changed in commit abc1234 (+1 -1)"
        );
        assert!(
            heads[2].starts_with("### tests/frame_test.rs:1-3"),
            "{heads:?}"
        );
        assert!(
            !heads
                .iter()
                .any(|head| head.contains("Cargo.lock") || head.contains("gone.rs")),
            "{heads:?}"
        );
        assert_eq!(heads[3], "### touched declarations and their callers");
        let notes = blocks[3].text();
        assert!(notes.contains("src/frame.rs:3-6 function clip"), "{notes}");
        assert!(
            notes.contains("callers of clip: src/main.rs:12 (in run)"),
            "{notes}"
        );
        assert!(
            !notes.contains("frame_test"),
            "a test is not a caller worth naming: {notes}"
        );
        assert!(
            blocks[0]
                .text()
                .contains("every changed source file has one beside it"),
            "{}",
            blocks[0].text()
        );
    }

    #[test]
    fn a_review_should_fall_back_to_the_latest_commit_and_say_nothing_without_a_change() {
        let world = World {
            changes: Some(change(vec![changed("src/a.rs", 'M', 1, 0, &[(1, 1)])])),
            ..World::default()
        }
        .with_file("src/a.rs", &["fn a() {}"]);
        // The fake has changes only for the latest commit: the working tree
        // the prompt asked about holds none.
        let blocks = blocks(
            &input(QuestionKind::Review, "review my changes", &[], &[], &[]),
            &world,
            soon(),
        );
        assert_eq!(heads(&blocks)[0], "### change summary — commit abc1234");
        assert!(
            super::blocks(
                &input(QuestionKind::Review, "review my changes", &[], &[], &[]),
                &World::default(),
                soon()
            )
            .is_empty()
        );
    }

    fn flow_world() -> World {
        let handler = hit("src/drop.ts", "onDrop", "function", 2, 8);
        let insert = hit("src/image.ts", "insertImage", "function", 1, 5);
        let helper = hit("src/image.ts", "resizeImage", "function", 7, 9);
        let tree = |root: &SymbolHit, rest: &[&SymbolHit]| -> Vec<FlowNode> {
            let mut nodes = vec![node(
                0,
                None,
                &root.name,
                &root.path,
                root.start_line,
                root.end_line,
            )];
            for child in rest {
                nodes.push(node(
                    1,
                    Some(0),
                    &child.name,
                    &child.path,
                    child.start_line,
                    child.end_line,
                ));
            }
            nodes
        };
        World {
            like: vec![handler.clone(), insert.clone(), helper.clone()],
            symbols: vec![handler.clone(), insert.clone(), helper.clone()],
            trees: vec![
                (handler.uid.clone(), tree(&handler, &[&insert, &helper])),
                (insert.uid.clone(), tree(&insert, &[&helper])),
                (helper.uid.clone(), tree(&helper, &[])),
            ],
            ..World::default()
        }
        .with_file(
            "src/drop.ts",
            &[
                "// drop",
                "",
                "function onDrop(event) {",
                "  const file = event.file;",
                "  insertImage(file);",
                "  resizeImage(file);",
                "  return file;",
                "}",
            ],
        )
        .with_file(
            "src/image.ts",
            &[
                "function insertImage(file) {",
                "  return file;",
                "}",
                "",
                "// helper",
                "// padding",
                "function resizeImage(file) {",
                "  return file;",
                "}",
            ],
        )
    }

    #[test]
    fn a_walkthrough_should_start_at_the_function_that_carries_the_rare_word_and_reaches_the_rest()
    {
        let world = flow_world();
        let blocks = blocks(
            &input(
                QuestionKind::Flow,
                "walk me through what happens when someone drops an image onto the canvas",
                &[],
                &[],
                &[],
            ),
            &world,
            soon(),
        );
        let heads = heads(&blocks);
        assert_eq!(heads[0], "### call trace, in call order");
        let trace = blocks[0].text();
        assert!(trace.contains("entry onDrop — src/drop.ts:2-8"), "{trace}");
        assert!(
            trace.contains("  calls insertImage — src/image.ts:1-5"),
            "{trace}"
        );
        assert!(
            trace.contains("  calls resizeImage — src/image.ts:7-9"),
            "{trace}"
        );
        assert!(
            heads.iter().any(|head| head.contains("onDrop definition")),
            "{heads:?}"
        );
        assert!(
            heads
                .iter()
                .any(|head| head.contains("insertImage definition")),
            "{heads:?}"
        );
    }

    #[test]
    fn a_walkthrough_should_use_the_path_the_chain_found_or_the_symbol_the_prompt_names() {
        let world = flow_world();
        let hops = strings(&["onDrop", "insertImage"]);
        let blocks = blocks(
            &input(
                QuestionKind::Flow,
                "how does onDrop reach insertImage",
                &[],
                &[],
                &hops,
            ),
            &world,
            soon(),
        );
        let trace = blocks[0].text();
        assert!(trace.contains("entry onDrop"), "{trace}");
        assert!(trace.contains("  calls insertImage"), "{trace}");
        assert!(
            !trace.contains("resizeImage"),
            "a found path lists its hops only: {trace}"
        );
        let names = strings(&["insertImage"]);
        let blocks = super::blocks(
            &input(
                QuestionKind::Flow,
                "walk me through insertImage",
                &names,
                &[],
                &[],
            ),
            &world,
            soon(),
        );
        assert!(blocks[0].text().contains("entry insertImage"));
        assert!(blocks[0].text().contains("  calls resizeImage"));
    }

    #[test]
    fn a_walkthrough_should_fall_back_to_the_meaning_leads_and_give_up_without_a_tree() {
        let mut world = flow_world();
        world.like.clear();
        let leads = vec![
            ("test_drop".to_string(), "tests/drop.test.ts".to_string()),
            ("onDrop".to_string(), "src/drop.ts".to_string()),
        ];
        let found = blocks(
            &input(
                QuestionKind::Flow,
                "walk me through a zzz",
                &[],
                &leads,
                &[],
            ),
            &world,
            soon(),
        );
        assert!(
            found[0].text().contains("entry onDrop"),
            "{}",
            found[0].text()
        );
        world.trees.clear();
        assert!(
            blocks(
                &input(
                    QuestionKind::Flow,
                    "walk me through a zzz",
                    &[],
                    &leads,
                    &[]
                ),
                &world,
                soon()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_kind_without_a_pack_and_an_expired_window_give_no_blocks() {
        let world = flow_world();
        for kind in [
            QuestionKind::Lookup,
            QuestionKind::Config,
            QuestionKind::Tests,
        ] {
            assert!(blocks(&input(kind, "x", &[], &[], &[]), &world, soon()).is_empty());
        }
        assert!(owns_the_pack(QuestionKind::Overview) && owns_the_pack(QuestionKind::Review));
        assert!(!owns_the_pack(QuestionKind::Flow) && !owns_the_pack(QuestionKind::Lookup));
        let expired = Instant::now();
        let none = blocks(
            &input(
                QuestionKind::Flow,
                "walk me through what happens when someone drops an image",
                &[],
                &[],
                &[],
            ),
            &world,
            expired,
        );
        assert!(none.is_empty());
    }

    // ---- history ---------------------------------------------------------------

    fn commit_hit(
        sha: &str,
        at: &str,
        subject: &str,
        path: Option<&str>,
        snippet: Option<&str>,
    ) -> HistoryHit {
        HistoryHit {
            sha: sha.to_string(),
            subject: subject.to_string(),
            at: at.to_string(),
            path: path.map(str::to_string),
            snippet: snippet.map(str::to_string),
        }
    }

    #[test]
    fn group_commits_should_put_the_commit_most_hits_name_first_and_collect_its_paths() {
        let hits = vec![
            commit_hit(
                "aaa",
                "2023-01-02T10:00:00Z",
                "ignore: skip unreachable",
                None,
                None,
            ),
            commit_hit(
                "bbb",
                "2024-05-06T10:00:00Z",
                "unrelated",
                Some("b.rs"),
                None,
            ),
            commit_hit(
                "aaa",
                "2023-01-02T10:00:00Z",
                "ignore: skip unreachable",
                Some("walk.rs"),
                None,
            ),
            commit_hit(
                "aaa",
                "2023-01-02T10:00:00Z",
                "ignore: skip unreachable",
                Some("walk.rs"),
                Some("+ let x = 1;"),
            ),
            commit_hit(
                "aaa",
                "2023-01-02T10:00:00Z",
                "ignore: skip unreachable",
                Some("dir.rs"),
                None,
            ),
        ];
        let commits = group_commits(&hits);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[0].date, "2023-01-02");
        assert_eq!(commits[0].hits, 4);
        // A path keeps the first changed line a hit gave it.
        assert_eq!(
            commits[0].touched,
            [
                ("walk.rs".to_string(), Some("+ let x = 1;".to_string())),
                ("dir.rs".to_string(), None)
            ]
        );
        assert_eq!(commits[1].sha, "bbb");
        let many: Vec<HistoryHit> = (0..MAX_COMMITS + 4)
            .map(|n| commit_hit(&format!("s{n}"), "2024-01-01", "x", None, None))
            .collect();
        assert_eq!(group_commits(&many).len(), MAX_COMMITS);
    }

    #[test]
    fn locate_changed_line_should_find_a_line_of_the_diff_in_the_current_file() {
        let source = strings(&[
            "fn a() {",
            "    if reachable(x) {",
            "        skip_it();",
            "    }",
            "}",
        ]);
        assert_eq!(
            locate_changed_line(&source, "+        skip_it();\n+    }"),
            Some(3)
        );
        assert_eq!(
            locate_changed_line(&source, "- gone_line_removed();\n+ if reachable(x) {"),
            Some(2)
        );
        // Too short to mean anything, or not in the file at all.
        assert_eq!(locate_changed_line(&source, "+ }"), None);
        assert_eq!(
            locate_changed_line(&source, "+ not in the file at all"),
            None
        );
    }

    #[test]
    fn history_lines_should_show_sha_date_subject_what_changed_and_where() {
        let commits = vec![Commit {
            sha: "435f59fc4b".into(),
            date: "2026-07-29".into(),
            subject: "ignore: skip loading unreachable ignore files".into(),
            touched: vec![
                (
                    "crates/ignore/src/walk.rs".into(),
                    Some("+ skip_it();".into()),
                ),
                ("crates/ignore/src/dir.rs".into(), None),
            ],
            changed: vec!["crates/ignore/src/walk.rs:639 +31 -3".into()],
            hits: 3,
        }];
        let lines = history_lines(&commits, &|path, _| {
            (path.ends_with("walk.rs")).then_some(642)
        });
        assert_eq!(
            lines,
            [
                "435f59fc4b 2026-07-29 ignore: skip loading unreachable ignore files",
                "  changed crates/ignore/src/walk.rs:639 +31 -3",
                "  touched crates/ignore/src/walk.rs:642",
                "  touched crates/ignore/src/dir.rs"
            ]
        );
        let undated = vec![Commit {
            date: String::new(),
            changed: Vec::new(),
            touched: Vec::new(),
            ..commits[0].clone()
        }];
        assert_eq!(
            history_lines(&undated, &|_, _| None)[0],
            "435f59fc4b ignore: skip loading unreachable ignore files"
        );
    }

    #[test]
    fn history_words_should_keep_the_raw_content_words_and_drop_what_only_asks_for_history() {
        let words = history_words(
            "when did ignore start skipping unreachable ignore files, which commit was it and what did it change",
            None,
        );
        assert_eq!(words, ["ignore", "skipping", "unreachable"]);
    }

    #[test]
    fn a_history_question_should_list_the_commits_with_what_they_changed() {
        let world = World {
            history: vec![
                commit_hit(
                    "435f59fc4b",
                    "2026-07-29T08:00:00Z",
                    "ignore: skip unreachable ignore files",
                    Some("walk.rs"),
                    Some("+        skip_it();"),
                ),
                commit_hit(
                    "1111111111",
                    "2020-01-01T08:00:00Z",
                    "docs: typo",
                    None,
                    None,
                ),
            ],
            changes: Some(Changes {
                label: "commit 435f59f".into(),
                subject: Some("435f59f ignore: skip unreachable ignore files".into()),
                files: vec![changed("walk.rs", 'M', 31, 3, &[(5, 9)])],
                truncated: false,
            }),
            ..World::default()
        }
        .with_file(
            "walk.rs",
            &[
                "fn walk() {",
                "    a();",
                "    b();",
                "    c();",
                "    skip_it();",
            ],
        );
        let typed = "when did ignore start skipping unreachable files, which commit was it";
        let out = blocks(
            &input(QuestionKind::History, typed, &[], &[], &[]),
            &world,
            soon(),
        );
        assert_eq!(out.len(), 1);
        let text = out[0].text();
        assert!(text.starts_with("### history — commits matching"), "{text}");
        assert!(
            text.contains("435f59fc4b 2026-07-29 ignore: skip unreachable ignore files"),
            "{text}"
        );
        assert!(text.contains("  changed walk.rs:5 +31 -3"), "{text}");
        assert!(text.contains("  touched walk.rs:5"), "{text}");
        // No history index: nothing is said.
        assert!(
            blocks(
                &input(QuestionKind::History, typed, &[], &[], &[]),
                &World::default(),
                soon()
            )
            .is_empty()
        );
        assert!(owns_the_pack(QuestionKind::History));
    }

    fn with_patch(mut file: ChangedFile, patch: &[&str]) -> ChangedFile {
        file.patch = strings(patch);
        file
    }

    #[test]
    fn added_test_names_should_read_the_cases_a_change_adds_to_test_files_only() {
        let changes = change(vec![
            with_patch(
                changed("tests/frame.test.ts", 'M', 4, 0, &[(1, 4)]),
                &[
                    "@@ -0,0 +1,4 @@",
                    "+  it(\"clips a child on the frame edge\", () => {",
                    "+    expect(x).toBe(1);",
                    "+  });",
                    "+  test(\"keeps bound text\", () => {});",
                ],
            ),
            with_patch(
                changed("tests/test_b.py", 'A', 2, 0, &[(1, 2)]),
                &[
                    "@@ -0,0 +1,2 @@",
                    "+def test_cut_removes_frame_children(self):",
                    "+    pass",
                ],
            ),
            with_patch(
                changed("src/frame.ts", 'M', 1, 0, &[(1, 1)]),
                &["@@ -0,0 +1 @@", "+it(\"not a test file\", () => {})"],
            ),
        ]);
        assert_eq!(
            added_test_names(&changes),
            [
                "clips a child on the frame edge",
                "keeps bound text",
                "test_cut_removes_frame_children"
            ]
        );
        let summary = summary_lines(&changes).join("\n");
        assert!(
            summary.contains("tests added: clips a child on the frame edge; keeps bound text; test_cut_removes_frame_children"),
            "{summary}"
        );
    }

    #[test]
    fn a_review_should_show_the_diff_of_the_changed_source_so_the_removed_lines_are_there() {
        let world = World {
            changes: Some(change(vec![
                with_patch(
                    changed("src/frame.rs", 'M', 1, 1, &[(4, 4)]),
                    &[
                        "@@ -4 +4 @@ pub fn clip(a: u8) -> u8 {",
                        "-    let b = a;",
                        "+    let b = a + 1;",
                    ],
                ),
                with_patch(
                    changed("tests/frame_test.rs", 'M', 1, 0, &[(1, 1)]),
                    &["@@ -0,0 +1 @@", "+fn t() {}"],
                ),
            ])),
            ..World::default()
        }
        .with_file(
            "src/frame.rs",
            &[
                "use x;",
                "",
                "",
                "pub fn clip(a: u8) -> u8 {",
                "    let b = a + 1;",
                "    b",
                "}",
            ],
        );
        let out = blocks(
            &input(
                QuestionKind::Review,
                "review the latest commit",
                &[],
                &[],
                &[],
            ),
            &world,
            soon(),
        );
        let heads = heads(&out);
        assert_eq!(heads[1], "### diff of src/frame.rs (+1 -1)");
        let diff = out[1].text();
        assert!(
            diff.contains("-    let b = a;\n+    let b = a + 1;"),
            "{diff}"
        );
        // Only source files get a diff block; the test's lines are in the summary.
        assert!(
            !heads
                .iter()
                .any(|head| head.starts_with("### diff of tests/")),
            "{heads:?}"
        );
    }

    #[test]
    fn named_commit_should_take_a_hex_id_with_a_digit_and_a_letter() {
        let world = World::default();
        let soon = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            named_commit("review commit 3FCE3B5 please", &world, soon).as_deref(),
            Some("3fce3b5")
        );
        // Words made of hex letters, and ids made of digits alone, name nothing.
        assert_eq!(
            named_commit("the defaced decade 1234567 bedfade", &world, soon),
            None
        );
        assert_eq!(named_commit("review the latest commit", &world, soon), None);
    }

    #[test]
    fn named_commit_should_find_a_quoted_title_and_prefer_the_exact_subject() {
        let soon = Instant::now() + Duration::from_secs(1);
        let world = World {
            git_log: Some(
                "aaa1111 ci: fix binary discovery again\nbbb2222 ci: fix binary discovery\n".into(),
            ),
            ..World::default()
        };
        assert_eq!(
            named_commit(
                "Review the commit titled 'ci: fix binary discovery'",
                &world,
                soon
            )
            .as_deref(),
            Some("bbb2222")
        );
        let loose = World {
            git_log: Some("ccc3333 ci: fix binary discovery v2\n".into()),
            ..World::default()
        };
        assert_eq!(
            named_commit("review \"ci: fix binary discovery\"", &loose, soon).as_deref(),
            Some("ccc3333"),
            "no exact subject: the first match"
        );
        let silent = World {
            git_log: Some(String::new()),
            ..World::default()
        };
        assert_eq!(
            named_commit("review 'a title nobody wrote'", &silent, soon),
            None
        );
    }

    #[test]
    fn a_review_of_a_named_commit_reads_that_commit_not_the_latest() {
        let soon = Instant::now() + Duration::from_secs(1);
        let world = World {
            changes: Some(Changes {
                label: "commit bbb2222".into(),
                subject: None,
                files: Vec::new(),
                truncated: false,
            }),
            git_log: Some("bbb2222 ci: fix binary discovery\n".into()),
            ..World::default()
        };
        let blocks = review_blocks(
            "Review the commit titled 'ci: fix binary discovery'",
            &world,
            soon,
        );
        assert_eq!(heads(&blocks)[0], "### change summary — commit bbb2222");
    }
}
