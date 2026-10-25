// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `name-files`: two locators the brief lacks, and one more for a question
//! about a module.
//!
//! (a) Adjacent prompt words joined into a code name ("smart case" ->
//! `SmartCase`, `smart_case`, `smart-case`), located with one search over
//! every compound, the definition shown. (b) A source file whose stem is a
//! word of the prompt ("config" -> `flags/config.rs`), shown whole when short.
//! (c) For a "which part / module / file / depends" question, the tracked
//! source files whose name shares two words with the prompt and the places
//! that import them. Blocks of the brief that share no content word with the
//! prompt and lie away from what was located are dropped.
//!
//! Ported from `probes/name-files/probe`. The script ran `rg`; the step walks
//! the tree with the `ignore` crate `rg` is built on (hidden files, ignore
//! files and the `*.rs *.ts *.tsx *.js *.py *.go *.rb` globs, in `--sort
//! path` order), so it searches the same files, untracked ones included. Two
//! shortcuts: a file is searched up to its first NUL byte, as `rg` does, and
//! the byte and row caps of the script's bounded reads of `rg` and `git
//! ls-files --stage` are counted on the rows they would have carried, not on
//! the 64 KiB reads. The index mode of a path is not read (a regular file
//! under `lstat` is what the script finally required).

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{escape, head, splitlines, stem_ing_es_s, subwords, universal_newlines};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "name-files",
    when_kinds: &[
        "config",
        "lookup",
        "tests",
        "impact",
        "diagnosis",
        "error-origin",
        "feature",
        "bugfix",
        "rationale",
        "other",
    ],
    skip_kinds: &["overview", "review", "flow", "history"],
    when_prompt: Some(r"(?i)\b(parts?|functions?|modules?|files?|depend(?:s|ed|ing)?)\b"),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Lead,
    run,
};

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "the", "and", "how", "is",
    "a", "of", "it", "defined", "define", "number", "returns", "return", "set", "get", "use",
    "used", "make", "could", "there",
];
const GENERIC: &[&str] = &[
    "for", "to", "in", "on", "my", "its", "keep", "file", "line", "code", "thing", "error",
    "change", "nothing", "make", "read", "ignored", "get", "files", "lines", "work", "works",
    "happens", "nothing", "time",
];
/// The globs of `-g`, and the extensions they stand for.
const SRC: [&str; 7] = ["*.rs", "*.ts", "*.tsx", "*.js", "*.py", "*.go", "*.rb"];
const SRC_EXTENSIONS: [&str; 7] = ["rs", "ts", "tsx", "js", "py", "go", "rb"];
const FILE_LINES: usize = 180;
const DEFINITION_LINES: usize = 60;
const MAX_FILE_LINES: usize = 220;
const MODULE_FILE_LINES: usize = 24;
const MAX_MODULES: usize = 2;
const MAX_MODULE_REFS: usize = 8;
const MODULE_SCAN_ROWS: usize = 400;
const MODULE_SCAN_BYTES: usize = 250_000;
const MODULE_OUT_CHARS: usize = 3_000;
const TRACKED_MAX_BYTES: usize = 2_000_000;
const TRACKED_MAX_FILES: usize = 20_000;
const MODULE_FILE_MAX_BYTES: u64 = 250_000;
/// Bytes of `git ls-files --stage -z` besides the path: mode, object name,
/// stage, tab and the NUL.
const STAGE_ROW_BYTES: usize = 51;
/// A line `rg -M 1200` prints, and a row the script keeps (`<= 1_300`).
const MAX_COLUMNS: usize = 1_200;
const MAX_ROW_BYTES: usize = 1_300;
/// Bytes of a source file that are searched or quoted.
const READ_LIMIT: u64 = 100_000_000;

static MODULE_INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:parts?|functions?|modules?|files?|depend(?:s|ed|ing)?)\b")
        .expect("the module-intent pattern is a literal")
});
static MODULE_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:mod|use|import|from|require|include)\b|::")
        .expect("the module-reference pattern is a literal")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|/)(?:tests?|__tests__|spec)(?:/|$)|(?:^|/)(?:test|spec)[^/]*\.")
        .expect("the test-path pattern is a literal")
});
static BRIEF_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^### (\S+)").expect("the brief-header pattern is a literal")
});

/// Adjacent prompt words joined into the names of a code definition.
struct Pair {
    camel: String,
    snake: String,
    kebab: String,
}

fn run(ctx: &Ctx) -> String {
    let prompt = ctx.prompt;
    let pairs = compounds(prompt);
    // Without git the script still searched the tree; it only lacked the index.
    let files = ctx.files().unwrap_or(&[]);
    let Some(listing) = source_files(ctx) else {
        return String::new();
    };
    let defs = definitions(ctx, &listing, &pairs);
    let mut found: Vec<String> = Vec::new();
    for (index, pair) in pairs.iter().enumerate() {
        for (path, line) in defs.get(&index).into_iter().flatten().take(2) {
            found.push(format!(
                "### {}:{line} — {} definition (named by the prompt: '{}')",
                path.trim_start_matches(['.', '/']),
                pair.camel,
                pair.kebab.replace('-', " ")
            ));
            found.extend(body(ctx, path, line.saturating_sub(3).max(1), DEFINITION_LINES));
        }
    }
    let rows = stage_rows(files);
    let safe = Safe::new(ctx.root());
    found.extend(module_evidence(ctx, &safe, &rows, prompt));
    if ctx.timed_out() {
        return String::new();
    }

    // A whole word only: `model` inside `ask_model_to_relay` names no file.
    let stems = prompt_stems(prompt);
    let near: Vec<&str> = BRIEF_HEADER
        .captures_iter(ctx.brief)
        .map(|caps| {
            let head = caps.get(1).map_or("", |m| m.as_str());
            let head = head.split(':').next().unwrap_or("");
            head.split(' ').next().unwrap_or("")
        })
        .collect();
    let mut candidates: Vec<(Reverse<usize>, String)> = Vec::new();
    let mut names: Vec<&str> = listing
        .iter()
        .map(|file| file.trim_start_matches(['.', '/']))
        .collect();
    names.sort_unstable();
    for name in names {
        let stem = file_stem(name);
        if !stems.contains(&stem.to_lowercase()) || name.to_lowercase().contains("test") {
            continue;
        }
        let shared = near
            .iter()
            .map(|other| common_dirs(name, other))
            .max()
            .unwrap_or(0);
        if shared > 0 {
            candidates.push((Reverse(shared), name.to_owned()));
        }
    }
    candidates.sort();
    if let Some((_, name)) = candidates.first()
        && let Some(text) = ctx.read(name, READ_LIMIT)
    {
        let count = line_count(&universal_newlines(&text));
        if count <= MAX_FILE_LINES {
            found.push(format!(
                "### {name}:1-{} — file named '{}' (a word of the prompt)",
                count.min(FILE_LINES),
                file_stem(name)
            ));
            found.extend(body(ctx, name, 1, FILE_LINES));
        }
    }
    if found.is_empty() {
        return String::new();
    }

    let want = words(prompt);
    let added: Vec<&str> = found
        .iter()
        .filter_map(|line| line.strip_prefix("### "))
        .map(|head| head.split(':').next().unwrap_or(""))
        .collect();
    let mut out = String::new();
    for block in ctx.brief.split("\n### ").skip(1) {
        let header = block.split('\n').next().unwrap_or("");
        let path = header
            .split(':')
            .next()
            .unwrap_or("")
            .split(' ')
            .next()
            .unwrap_or("");
        // Noise is a block that names none of the prompt's words and lies away
        // from what was just located.
        if words(header).is_disjoint(&want) && !added.iter().any(|other| common_dirs(path, other) > 0)
        {
            out.push_str(&format!("drop: ^{}$\n", escape(header)));
        }
    }
    out.push_str(&found.join("\n"));
    out.push('\n');
    out
}

/// The stemmed words of three letters or more that are not function words:
/// the runs of letters the script's `[A-Za-z][a-z]+|[A-Z]+(?![a-z])` finds.
fn words(text: &str) -> BTreeSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = BTreeSet::new();
    let mut at = 0;
    while at < chars.len() {
        let mut end = at;
        if chars[at].is_ascii_alphabetic() && chars.get(at + 1).is_some_and(char::is_ascii_lowercase)
        {
            end = at + 2;
            while chars.get(end).is_some_and(char::is_ascii_lowercase) {
                end += 1;
            }
        } else if chars[at].is_ascii_uppercase() {
            end = at + 1;
            while chars.get(end).is_some_and(char::is_ascii_uppercase) {
                end += 1;
            }
            // `[A-Z]+(?![a-z])`: the last capital starts the next word.
            if chars.get(end).is_some_and(char::is_ascii_lowercase) {
                end -= 1;
            }
        }
        if end == at {
            at += 1;
            continue;
        }
        let word: String = chars[at..end].iter().collect();
        let lower = word.to_lowercase();
        if end - at > 2 && !STOP.contains(&lower.as_str()) && !GENERIC.contains(&lower.as_str()) {
            out.insert(stem_ing_es_s(&word));
        }
        at = end;
    }
    out
}

/// The identifier or phrase as lowercase words, ordinary English plurals
/// normalised alike.
fn identifier_parts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for mut word in subwords(text) {
        let length = word.len();
        if length > 4 && word.ends_with("ies") {
            word.truncate(length - 3);
            word.push('y');
        } else if length > 5 && ["ches", "shes", "sses", "xes", "zes"].iter().any(|s| word.ends_with(s)) {
            word.truncate(length - 2);
        } else if length > 3 && word.ends_with('s') && !word.ends_with("ss") {
            word.truncate(length - 1);
        }
        if word.len() > 2 {
            out.push(word);
        }
    }
    out
}

/// The whole-word lowercase tokens of four letters or more that name a file
/// stem: `(?<!\w)[a-z]{4,}(?!\w)` over the lowercased prompt.
fn prompt_stems(prompt: &str) -> BTreeSet<String> {
    let lower = prompt.to_lowercase();
    lower
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|run| run.len() >= 4 && run.chars().all(|ch| ch.is_ascii_lowercase()))
        .filter(|run| !STOP.contains(run) && !GENERIC.contains(run))
        .map(ToString::to_string)
        .collect()
}

/// The pairs of adjacent lowercase words of the prompt that can name code.
fn compounds(prompt: &str) -> Vec<Pair> {
    let lower = prompt.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|ch: char| !ch.is_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    let skipped = |word: &str| STOP.contains(&word) || GENERIC.contains(&word) || word.len() < 3;
    tokens
        .windows(2)
        .filter(|pair| !skipped(pair[0]) && !skipped(pair[1]))
        .map(|pair| {
            let (a, b) = (pair[0], pair[1]);
            let capital = |word: &str| {
                let (first, rest) = word.split_at(1);
                first.to_uppercase() + rest
            };
            Pair {
                camel: capital(a) + &capital(b),
                snake: format!("{a}_{b}"),
                kebab: format!("{a}-{b}"),
            }
        })
        .collect()
}

/// The source files `rg --files` lists for the script's globs, in `--sort
/// path` order; `None` when the walk runs out of time.
fn source_files(ctx: &Ctx) -> Option<Vec<String>> {
    let root = ctx.root();
    let mut globs = OverrideBuilder::new(root);
    globs.add("!.pixel").ok()?;
    for glob in SRC {
        globs.add(glob).ok()?;
    }
    let mut walk = WalkBuilder::new(root);
    walk.overrides(globs.build().ok()?)
        .sort_by_file_path(Path::cmp)
        .add_custom_ignore_filename(".rgignore");
    let mut out = Vec::new();
    for entry in walk.build() {
        if ctx.timed_out() {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(rel) = entry.path().strip_prefix(root).ok().and_then(Path::to_str) {
            out.push(rel.to_owned());
        }
    }
    Some(out)
}

/// `text` up to its first NUL byte: `rg` stops searching a binary file there.
fn text_part(text: &str) -> &str {
    text.split('\0').next().unwrap_or("")
}

/// One search for the definition of every compound, grouped by pair index.
/// Every camel and snake spelling joins one alternation, and a hit is
/// attributed to the pair whose spelling it carries (longest spelling first,
/// so `BookingAvailability` wins over a prefix).
fn definitions(ctx: &Ctx, listing: &[String], pairs: &[Pair]) -> BTreeMap<usize, Vec<(String, usize)>> {
    let spellings: Vec<(usize, &str)> = pairs
        .iter()
        .enumerate()
        .flat_map(|(index, pair)| [(index, pair.camel.as_str()), (index, pair.snake.as_str())])
        .collect();
    let mut found: BTreeMap<usize, Vec<(String, usize)>> = BTreeMap::new();
    if spellings.is_empty() {
        return found;
    }
    let mut ordered: Vec<&str> = spellings
        .iter()
        .map(|(_, word)| *word)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ordered.sort_by_key(|word| Reverse(word.len()));
    let alternatives: Vec<String> = ordered.iter().map(|word| regex::escape(word)).collect();
    let Ok(line_rx) = Regex::new(&format!(
        r"(struct|class|fn|def|const|enum|interface|type)\s+({})\b",
        alternatives.join("|")
    )) else {
        return found;
    };
    let word_rxs: Vec<Option<Regex>> = alternatives
        .iter()
        .map(|word| Regex::new(&format!(r"\b{word}\b")).ok())
        .collect();
    for path in listing {
        if ctx.timed_out() {
            return BTreeMap::new();
        }
        let Some(text) = ctx.read(path, READ_LIMIT) else {
            continue;
        };
        let text = text_part(&text);
        // Cheap and a superset: `\s` here also crosses lines.
        if !line_rx.is_match(text) {
            continue;
        }
        for (number, line) in text.split('\n').enumerate() {
            if !line_rx.is_match(line) {
                continue;
            }
            let carried = ordered
                .iter()
                .zip(&word_rxs)
                .find(|(_, rx)| rx.as_ref().is_some_and(|rx| rx.is_match(line)));
            if let Some((word, _)) = carried {
                for (index, _) in spellings.iter().filter(|(_, spelling)| spelling == word) {
                    found.entry(*index).or_default().push((path.clone(), number + 1));
                }
            }
        }
    }
    found
}

/// `count` numbered lines of `path` from line `lo`, each cut to 160 characters.
fn body(ctx: &Ctx, path: &str, lo: usize, count: usize) -> Vec<String> {
    let Some(text) = ctx.read(path, READ_LIMIT) else {
        return Vec::new();
    };
    let text = universal_newlines(&text);
    let source = splitlines(&text);
    let hi = source.len().min(lo + count - 1);
    (lo..=hi)
        .map(|number| format!("{number}| {}", head(source[number - 1], 160)))
        .collect()
}


/// The file name without its last extension.
fn file_stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
}

/// The lines Python's `for _ in open(...)` yields on text with `\n` line ends.
fn line_count(text: &str) -> usize {
    text.matches('\n').count() + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

/// The leading directories two paths share: a file named by a prompt word is
/// a lead only beside what the brief shows.
fn common_dirs(a: &str, b: &str) -> usize {
    let dirs = |path: &str| -> Vec<String> {
        let mut parts: Vec<&str> = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        parts.pop();
        let mut out = Vec::new();
        if path.starts_with('/') {
            out.push("/".to_owned());
        }
        out.extend(parts.into_iter().map(ToString::to_string));
        out
    };
    dirs(a)
        .iter()
        .zip(dirs(b).iter())
        .take_while(|(x, y)| x == y)
        .count()
}

/// The index records the script reads from `git ls-files --stage -z`: the
/// first [`TRACKED_MAX_FILES`], and no more than fit [`TRACKED_MAX_BYTES`].
fn stage_rows(files: &[String]) -> Vec<&str> {
    let mut used = 0;
    let mut rows = Vec::new();
    for file in files.iter().take(TRACKED_MAX_FILES) {
        used += STAGE_ROW_BYTES + file.len();
        if used > TRACKED_MAX_BYTES {
            break;
        }
        rows.push(file.as_str());
    }
    rows
}

/// Whether a tracked path is a regular file under the repository, no link on
/// the way: a directory known to be real is not looked at twice.
struct Safe<'a> {
    root: &'a Path,
    real_dirs: RefCell<HashMap<String, bool>>,
}

impl<'a> Safe<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            real_dirs: RefCell::new(HashMap::new()),
        }
    }

    fn regular(&self, rel: &str) -> bool {
        if rel.starts_with('/') || rel.split('/').any(|part| part == "..") {
            return false;
        }
        let Some((dirs, _)) = rel.rsplit_once('/') else {
            return self.is_regular(rel);
        };
        let mut prefix = String::new();
        for part in dirs.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let known = self.real_dirs.borrow().get(&prefix).copied();
            let real = known.unwrap_or_else(|| {
                let real = std::fs::symlink_metadata(self.root.join(&prefix))
                    .is_ok_and(|meta| !meta.file_type().is_symlink());
                self.real_dirs.borrow_mut().insert(prefix.clone(), real);
                real
            });
            if !real {
                return false;
            }
        }
        self.is_regular(rel)
    }

    fn is_regular(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.root.join(rel)).is_ok_and(|meta| meta.file_type().is_file())
    }
}

fn is_source(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SRC_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// The tracked source files sharing at least two words with the prompt, best
/// first, for an explicit module, use or dependency question: `(path, stem)`.
fn module_paths(safe: &Safe, rows: &[&str], prompt: &str) -> Vec<(String, String)> {
    if !MODULE_INTENT.is_match(prompt) {
        return Vec::new();
    }
    let prompt_words: BTreeSet<String> = identifier_parts(prompt)
        .into_iter()
        .filter(|word| !STOP.contains(&word.as_str()))
        .collect();
    if prompt_words.len() < 2 {
        return Vec::new();
    }
    let mut candidates: Vec<(Reverse<usize>, usize, &str, &str)> = Vec::new();
    for rel in rows {
        if !is_source(rel) || TEST_PATH.is_match(rel) {
            continue;
        }
        let stem = file_stem(rel);
        let parts: BTreeSet<String> = identifier_parts(stem).into_iter().collect();
        let shared = parts.intersection(&prompt_words).count();
        if parts.len() >= 2 && shared >= 2 && safe.regular(rel) {
            candidates.push((Reverse(shared), parts.len() - shared, rel, stem));
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .take(MAX_MODULES)
        .map(|(_, _, rel, stem)| ((*rel).to_owned(), stem.to_owned()))
        .collect()
}

/// The first line-matching places that import a module in the tracked
/// sources, one per file, at most [`MAX_MODULE_REFS`]: `(path, line, module)`.
fn module_references(ctx: &Ctx, safe: &Safe, rows: &[&str], modules: &[&str]) -> Vec<(String, usize, String)> {
    let sources: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|rel| is_source(rel) && safe.regular(rel))
        .collect();
    // `rg` names no file when it is given exactly one.
    if modules.is_empty() || sources.len() < 2 {
        return Vec::new();
    }
    // `rg -F -m 2 -M 1200 --no-max-columns-preview` over the sources, one
    // thread: two matching lines per file, a long one printed as a marker.
    // The script reads the first `MODULE_SCAN_BYTES` bytes of its output and
    // keeps the rows that end inside them, at most `MODULE_SCAN_ROWS`.
    let mut hits: Vec<(String, usize, String)> = Vec::new();
    let mut bytes = 0;
    'files: for rel in sources {
        if ctx.timed_out() {
            return Vec::new();
        }
        let Some(text) = ctx.read(rel, READ_LIMIT) else {
            continue;
        };
        let matching = text_part(&text)
            .split('\n')
            .enumerate()
            .filter(|(_, line)| modules.iter().any(|module| line.contains(module)))
            .take(2);
        for (number, line) in matching {
            let shown = if line.len() + 1 > MAX_COLUMNS {
                "[Omitted long matching line]"
            } else {
                line
            };
            let row = rel.len() + 1 + (number + 1).to_string().len() + 1 + shown.len();
            bytes += row + 1;
            if bytes > MODULE_SCAN_BYTES || hits.len() >= MODULE_SCAN_ROWS {
                break 'files;
            }
            // A path with a colon splits wrong in the script and is no hit.
            if row <= MAX_ROW_BYTES && !rel.contains(':') {
                hits.push((rel.to_owned(), number + 1, shown.to_owned()));
            }
        }
    }
    let mut refs: Vec<(String, usize, String)> = hits
        .into_iter()
        .filter_map(|(rel, number, text)| {
            let module = modules.iter().find(|module| text.contains(**module))?;
            (!TEST_PATH.is_match(&rel) && MODULE_REFERENCE.is_match(&text) && safe.regular(&rel))
                .then(|| (rel, number, (*module).to_owned()))
        })
        .collect();
    refs.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut seen = BTreeSet::new();
    refs.retain(|(rel, _, _)| seen.insert(rel.clone()));
    refs.truncate(MAX_MODULE_REFS);
    refs
}

/// A short source excerpt and the exact imports of a module the prompt names.
fn module_evidence(ctx: &Ctx, safe: &Safe, rows: &[&str], prompt: &str) -> Vec<String> {
    let candidates = module_paths(safe, rows, prompt);
    if candidates.is_empty() {
        return Vec::new();
    }
    let modules: Vec<&str> = candidates.iter().map(|(_, name)| name.as_str()).collect();
    let refs = module_references(ctx, safe, rows, &modules);
    let mut out: Vec<String> = Vec::new();
    let mut used = 0;
    let mut add = |block: Vec<String>| {
        let cost = block.join("\n").chars().count() + 1;
        if used + cost <= MODULE_OUT_CHARS {
            out.extend(block);
            used += cost;
        }
    };
    for (rel, _) in &candidates {
        if !safe.regular(rel) {
            continue;
        }
        let Some(source) = bounded_lines(ctx, rel) else {
            continue;
        };
        let hi = source.len().min(MODULE_FILE_LINES);
        let mut block = vec![format!("### {rel}:1-{hi} — module named by the prompt")];
        block.extend(
            source[..hi]
                .iter()
                .enumerate()
                .map(|(index, line)| format!("{}| {}", index + 1, head(line, 160))),
        );
        add(block);
    }
    for (rel, number, name) in &refs {
        let Some(source) = bounded_lines(ctx, rel) else {
            continue;
        };
        if *number > source.len() {
            continue;
        }
        add(vec![
            format!("### {rel}:{number}-{number} — source reference to {name}"),
            format!("{number}| {}", head(&source[number - 1], 160)),
        ]);
    }
    out
}

/// The lines of a small regular tracked file, `None` past
/// [`MODULE_FILE_MAX_BYTES`].
fn bounded_lines(ctx: &Ctx, rel: &str) -> Option<Vec<String>> {
    let text = ctx.read(rel, MODULE_FILE_MAX_BYTES)?;
    Some(splitlines(&text).into_iter().map(ToString::to_string).collect())
}

#[cfg(test)]
mod tests;
