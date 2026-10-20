// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The region pack: the source an agent reads before it answers, shown to it
//! up front. For the symbols a prompt is about it carries the definition with
//! its doc comment and body, the usage sites each in the function that holds
//! them, and the tests by name, as line-numbered blocks of exact source under
//! one directive line. A census of vanilla sessions found the same sequence
//! behind most answers (search, read the definition, read a usage, answer);
//! with the regions in the prompt and the directive, agents answered without
//! a single tool call.
//!
//! The chain gathers the evidence (a graph lookup, a text search, file reads
//! behind [`Evidence`]); everything that decides what is shown is a pure
//! function in this file: which symbols are targets, where a definition ends,
//! which usage sites come first, how the blocks are laid out and cut into the
//! parts a host's per-hook cap allows. The text may exceed one hook's cap:
//! [`Pack::text`] returns it whole and [`split_parts`] cuts it at block
//! boundaries.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;

use super::answer;
use super::chain::{BRIEF_TAG, Evidence, RichHit, SymbolHit};
use super::decision_log;
use super::sent::Seen;

/// Environment kill switch of the pack (`0`, `false` or `off`): the brief is
/// then the packet it was before the pack existed.
pub(crate) const PACK_ENV: &str = "PIXEL_BRIEF_PACK";
/// Environment override of the hook parts the pack may fill (`1` to
/// [`MAX_PARTS`]). Without it the count is the `brief_parts` setting, the
/// number of hooks `pixel install` registers.
pub(crate) const PARTS_ENV: &str = "PIXEL_BRIEF_PARTS";
/// Characters of one delivered part: under the 10 000 a host caps one hook at.
pub(crate) const PART_CHARS: usize = 9_800;
/// Parts a pack may fill: the most hooks `brief_parts` allows.
pub(crate) const MAX_PARTS: usize = pixel_setup::config::BRIEF_PARTS_MAX as usize;
/// The line the pack ends on.
pub(crate) const DIRECTIVE: &str =
    "Answer from this context. Use a tool only if something you need is not shown above.";
/// The last line of a pack made of the regions nearest the question, with no
/// symbol or passage the prompt named behind them: a lead, not an answer.
pub(crate) const LEAD: &str = "These are the regions closest to the question, not a verified answer. If they do not show what you need, search as usual.";
/// The last line of a pack the window ended before it was complete.
const INCOMPLETE: &str =
    "pack incomplete (time window ended): open a file for anything not shown above.";
/// Symbols a pack is about.
const MAX_TARGETS: usize = 3;
/// Identifier anchors looked up.
const MAX_NAMES: usize = 3;
/// Definitions of one name shown when the name is ambiguous.
const NAME_DEFINITIONS: usize = 2;
/// A name with more definitions than this is too generic to be a target.
const AMBIGUOUS_DEFINITIONS: usize = 6;
/// Lines of a definition block (the doc comment above it excluded).
const MAX_DEF_LINES: usize = 60;
/// Doc-comment and attribute lines kept above a definition.
const MAX_DOC_LINES: usize = 6;
/// Usage sites shown per symbol.
const MAX_SITES: usize = 8;
/// Usage sites of the symbol a region stands for.
const REGION_SITES: usize = 4;
/// Lines around a usage site, inside its function.
const SITE_BEFORE: u64 = 2;
const SITE_AFTER: u64 = 2;
/// Tests listed per symbol.
const MAX_TESTS: usize = 6;
/// Characters of one source line.
const MAX_LINE_CHARS: usize = 160;
/// Characters of one block: a block is never cut across parts, so it must
/// fit in one.
const MAX_BLOCK_CHARS: usize = 8_000;
/// Lines of a file read to find what encloses a site.
const MAX_FILE_LINES: u64 = 20_000;
/// Files one pack reads.
const MAX_FILES: usize = 16;
/// Longest declaration looked for around a match.
const LONG_DECL_LINES: usize = 400;
/// Lines of a long function shown around a match in it.
const LONG_BEFORE: u64 = 20;
const LONG_AFTER: u64 = 30;
/// Lines of a region around a match no declaration encloses.
const MATCH_BEFORE: u64 = 6;
const MATCH_AFTER: u64 = 14;
/// Width reserved for the line that opens a continuation part.
const CONTINUATION_CHARS: usize = 48;

/// Characters of a pack the `pixel brief` command prints when nothing says
/// otherwise: the OpenCode extension that runs it drops a brief over
/// 4 000 bytes.
pub(crate) const CLI_CHARS: usize = 3_900;
const _: () = assert!(CLI_CHARS < 4_000);

/// How much a pack may be: `parts` hook parts of at most `chars` characters
/// each; none at all when the pack is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Limit {
    pub(crate) parts: usize,
    pub(crate) chars: usize,
}

impl Limit {
    /// `parts` parts of [`PART_CHARS`].
    pub(crate) const fn of(parts: usize) -> Self {
        Self {
            parts,
            chars: PART_CHARS,
        }
    }

    pub(crate) const fn on(self) -> bool {
        self.parts > 0
    }
}

/// The limit the switches give. Off wins. [`PARTS_ENV`], when set, is the
/// host's own count. Without it a hook gets the `brief_parts` setting
/// (`setting`, the hooks `pixel install` registered), and the `brief` command
/// (which OpenCode runs) the size it accepts. Pure: the caller reads
/// the environment and the setting.
pub(crate) fn limit_for(
    pack: Option<&str>,
    parts: Option<&str>,
    command: bool,
    setting: u8,
) -> Limit {
    if !decision_log::enabled(pack) {
        return Limit::default();
    }
    match parts.and_then(|value| value.trim().parse::<usize>().ok()) {
        Some(count) => Limit::of(count.clamp(1, MAX_PARTS)),
        // The `brief` command fits one 4 000-byte message unless its caller
        // asked for more parts (`pixel brief --parts N`).
        None if command && setting <= 1 => Limit {
            parts: 1,
            chars: CLI_CHARS,
        },
        None => Limit::of(usize::from(setting).clamp(1, MAX_PARTS)),
    }
}

/// [`limit_for`] of the process environment; `setting` is the hooks that run
/// this brief (`--part K/N`: `N`, and 1 for a host with a single hook).
pub(crate) fn limit_from_env(command: bool, setting: u8) -> Limit {
    limit_for(
        std::env::var(PACK_ENV).ok().as_deref(),
        std::env::var(PARTS_ENV).ok().as_deref(),
        command,
        setting,
    )
}

/// One block of the pack: a `### ` head line and the lines under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Block {
    head: String,
    body: Vec<String>,
}

impl Block {
    /// A block whose body is cut, at a line, to [`MAX_BLOCK_CHARS`].
    pub(crate) fn new(head: String, body: Vec<String>) -> Self {
        let mut size = head.len() + 5;
        let mut kept = Vec::with_capacity(body.len());
        for line in body {
            size += line.len() + 1;
            if size > MAX_BLOCK_CHARS {
                kept.push("… (block cut to fit)".to_string());
                break;
            }
            kept.push(line);
        }
        Self { head, body: kept }
    }

    /// This block with `map` applied to each line of its body.
    pub(crate) fn map_body(&mut self, map: impl Fn(&str) -> String) {
        for line in &mut self.body {
            *line = map(line);
        }
    }

    pub(crate) fn text(&self) -> String {
        let mut text = format!("### {}", self.head);
        for line in &self.body {
            text.push('\n');
            text.push_str(line);
        }
        text
    }
}

/// What a candidate block is, for the header counts and the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Definition,
    Usage,
    Tests,
    TestBody,
    Facts,
    /// A block a locator found for a question about a setting or about tests
    /// (see `locator`): shown after the symbols' own blocks.
    Locator,
}

/// A block the pack may carry, and why it ranks where it does.
#[derive(Clone, Debug)]
struct Candidate {
    /// Lower is kept first when the parts are full.
    priority: usize,
    /// Display order: target, section, first line.
    order: (usize, usize, u64),
    part: Part,
    block: Block,
}

/// The pack: blocks of exact source under a header and a directive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pack {
    searched: String,
    blocks: Vec<Block>,
    complete: bool,
    /// A symbol or a passage the prompt named (or its words spelled) stands
    /// behind the blocks, not only the best-matching regions.
    anchored: bool,
    targets: usize,
    definitions: usize,
    /// Blocks a locator contributed.
    locators: usize,
    fallback: bool,
}

impl Pack {
    /// At least one definition or usage block: a pack of facts alone is not
    /// the regions an agent would have read.
    pub(crate) fn has_evidence(&self) -> bool {
        self.definitions > 0
            || self.locators > 0
            || self.blocks.iter().any(|b| b.head.contains(" usage"))
    }

    /// The files the blocks show a range of, in order and without repeats.
    pub(crate) fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for (path, _, _) in self.blocks.iter().filter_map(block_range) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    /// The `(path, first line, last line)` of every block that shows a range:
    /// what this pack sends, for the session's record.
    pub(crate) fn ranges(&self) -> Vec<(String, u64, u64)> {
        self.blocks.iter().filter_map(block_range).collect()
    }

    /// Symbols the pack is about.
    pub(crate) const fn targets(&self) -> usize {
        self.targets
    }

    /// Blocks a locator contributed.
    pub(crate) const fn located(&self) -> usize {
        self.locators
    }

    /// The text search stood in for a graph that could not answer.
    pub(crate) const fn fallback(&self) -> bool {
        self.fallback
    }

    /// Keep the age warning even when it costs the final whole block. A
    /// stale pack is a lead to verify, never an instruction to trust old rows.
    pub(crate) fn text_with_notice(&self, notice: &str, limit: Limit) -> Option<String> {
        let mut pack = self.clone();
        pack.searched = format!("{notice}\n{}", pack.searched);
        while !pack.blocks.is_empty() {
            let text = pack.text_hedged(true);
            if fits(&text, limit) {
                return Some(text);
            }
            pack.blocks.pop();
            pack.complete = false;
        }
        None
    }

    /// The whole text, whatever its length: the tag line, the header, the
    /// blocks, then the directive (or the note that the window ended).
    pub(crate) fn text(&self) -> String {
        self.text_hedged(false)
    }

    /// [`Pack::text`]; `hedge` (the packet it replaces was cut short, so the
    /// search behind it may have missed something) ends a complete pack on the
    /// lead line instead of the directive.
    pub(crate) fn text_hedged(&self, hedge: bool) -> String {
        let mut lines = vec![BRIEF_TAG.to_string(), self.searched.clone()];
        lines.extend(self.blocks.iter().map(Block::text));
        lines.push(
            match (self.complete, self.anchored && !hedge) {
                (true, true) => DIRECTIVE,
                (true, false) => LEAD,
                (false, _) => INCOMPLETE,
            }
            .to_string(),
        );
        lines.join("\n")
    }
}

/// `text` cut at block boundaries into parts of at most `max` characters.
/// The first part is the text up to the break; every later part opens on a
/// line that names its place (`part 2/3`), so a host that delivers them as
/// separate contexts keeps them readable. The directive line stays with the
/// last block it follows unless that overflows, then it travels alone. A
/// block longer than `max` is cut at a line.
pub(crate) fn split_parts(text: &str, max: usize) -> Vec<String> {
    let mut segments: Vec<Vec<&str>> = vec![Vec::new()];
    let all: Vec<&str> = text.split('\n').collect();
    for (index, line) in all.iter().enumerate() {
        let tail = index + 1 == all.len() && [DIRECTIVE, LEAD, INCOMPLETE].contains(line);
        if line.starts_with("### ") || tail {
            segments.push(Vec::new());
        }
        if let Some(segment) = segments.last_mut() {
            segment.push(line);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for segment in segments.iter().filter(|segment| !segment.is_empty()) {
        let mut rows = segment.as_slice();
        while !rows.is_empty() {
            let reserve = if parts.is_empty() {
                0
            } else {
                CONTINUATION_CHARS + 1
            };
            let room = max.saturating_sub(reserve);
            let whole = rows.join("\n");
            if current.is_empty() && whole.len() <= room {
                current = whole;
                rows = &[];
            } else if !current.is_empty() && current.len() + 1 + whole.len() <= max {
                current.push('\n');
                current.push_str(&whole);
                rows = &[];
            } else if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            } else {
                // A segment that cannot fit a part of its own: cut it at a line.
                let mut taken = 0;
                let mut size = 0;
                for row in rows {
                    if taken > 0 && size + row.len() + 1 > room {
                        break;
                    }
                    size += row.len() + 1;
                    taken += 1;
                }
                current = rows[..taken].join("\n");
                rows = &rows[taken..];
                parts.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    let count = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                part
            } else {
                format!(
                    "{BRIEF_TAG} region pack, part {}/{count}\n{part}",
                    index + 1
                )
            }
        })
        .collect()
}

/// Whether the text fits `parts` parts.
fn fits(text: &str, limit: Limit) -> bool {
    let cut = split_parts(text, limit.chars);
    cut.len() <= limit.parts && cut.iter().all(|part| part.len() <= limit.chars)
}

// ---------------------------------------------------------------------------
// Pure selection: definitions, sites, windows, tests.
// ---------------------------------------------------------------------------

/// Where a symbol is declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefSite {
    pub(crate) path: String,
    pub(crate) start: u64,
    /// The last line when the graph knew it; `None` for a text match.
    pub(crate) end: Option<u64>,
    pub(crate) kind: String,
}

impl From<&SymbolHit> for DefSite {
    fn from(hit: &SymbolHit) -> Self {
        Self {
            path: hit.path.clone(),
            start: hit.start_line,
            end: Some(hit.end_line),
            kind: hit.kind.clone(),
        }
    }
}

/// Words that may precede a declaration keyword.
const MODIFIERS: &str = r"(?:(?:pub(?:\([^)]*\))?|export|default|static|final|public|private|protected|readonly|async|unsafe|extern|abstract|declare|override)\s+)*";

/// How a name is declared, compiled once for a search's worth of rows.
struct Declaration {
    /// A declaration in the shape of the common languages (a keyword and the
    /// name).
    strong: Regex,
    /// An assignment to the name at the start of a line (`NAME = …`,
    /// `NAME: T = …`): a declaration in the languages that declare by
    /// assigning.
    weak: Regex,
}

impl Declaration {
    /// `None` when the name cannot be matched (it never fails for an escaped
    /// name; the option keeps the lookup panic-free).
    fn of(name: &str) -> Option<Self> {
        let name = regex::escape(name);
        let strong = Regex::new(&format!(
            r"(?x)(?:^|[^\w])(?:{MODIFIERS})(?:
                (?P<fn>fn|def|function|func)\s*(?:\([^)]*\)\s*)?{name}\b
              | (?P<ty>struct|enum|trait|type|union|class|interface|mod|impl|module|namespace)\s+{name}\b
              | (?P<val>const|static|let|var|val)\s+(?:mut\s+)?{name}\b
            )"
        ))
        .ok()?;
        let weak = Regex::new(&format!(r"^\s*{MODIFIERS}{name}\s*(?::[^=]+)?=[^=]")).ok()?;
        Some(Self { strong, weak })
    }

    /// The kind of declaration `text` makes, and whether it is the strong
    /// shape.
    fn classify(&self, text: &str) -> Option<(&'static str, bool)> {
        if let Some(caps) = self.strong.captures(text) {
            let kind = if caps.name("fn").is_some() {
                "function"
            } else if caps.name("ty").is_some() {
                "type"
            } else {
                "value"
            };
            return Some((kind, true));
        }
        self.weak.is_match(text).then_some(("value", false))
    }
}

/// The kind of declaration `text` makes of `name`, `None` when it makes none.
/// A strong match names its keyword; an assignment is a `value`.
#[cfg(test)]
pub(crate) fn classify_definition(text: &str, name: &str) -> Option<&'static str> {
    Declaration::of(name)?.classify(text).map(|(kind, _)| kind)
}

/// Declarations of `name` among search rows, by their matched text: the
/// strong tier alone when any row has it, else the assignments. The stand-in
/// for a graph lookup that could not answer.
pub(crate) fn text_definitions(rows: &[RichHit], name: &str) -> Vec<DefSite> {
    let Some(declaration) = Declaration::of(name) else {
        return Vec::new();
    };
    let found: Vec<(DefSite, bool)> = rows
        .iter()
        .filter(|row| !is_noise_path(&row.path) && row.line > 0)
        .filter_map(|row| {
            let (kind, strong) = declaration.classify(row.text.as_deref()?)?;
            Some((
                DefSite {
                    path: row.path.clone(),
                    start: row.line,
                    end: None,
                    kind: kind.to_string(),
                },
                strong,
            ))
        })
        .collect();
    let any_strong = found.iter().any(|(_, strong)| *strong);
    found
        .into_iter()
        .filter(|(_, strong)| *strong || !any_strong)
        .map(|(site, _)| site)
        .collect()
}

/// A path whose lines a question is not about: build output and
/// credential-shaped files.
pub(crate) fn is_noise_path(path: &str) -> bool {
    super::chain::is_generated(path)
        || pixel_index::index::credential_path(std::path::Path::new(path))
}

/// How much a declaration kind is the thing a question names: types and
/// functions before values before members.
fn kind_weight(kind: &str) -> u8 {
    match kind {
        "function" | "method" | "class" | "struct" | "enum" | "trait" | "interface" | "type"
        | "const" | "value" => 0,
        "variant" | "module" => 1,
        _ => 2,
    }
}

/// `sites` ranked for display: the file a path anchor names, then code before
/// tests and docs, then the kind, then the order found.
pub(crate) fn rank_defs(
    sites: Vec<DefSite>,
    path_anchors: &[String],
    wants_tests: bool,
    wants_docs: bool,
) -> Vec<DefSite> {
    let anchored = |site: &DefSite| {
        path_anchors.iter().any(|anchor| {
            std::path::Path::new(&site.path).ends_with(anchor)
                || std::path::Path::new(anchor).ends_with(&site.path)
        })
    };
    let mut ranked: Vec<(usize, DefSite)> = sites.into_iter().enumerate().collect();
    ranked.sort_by_key(|(index, site)| {
        (
            !anchored(site),
            answer::demoted(&site.path, wants_tests, wants_docs),
            kind_weight(&site.kind),
            *index,
        )
    });
    ranked.into_iter().map(|(_, site)| site).collect()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether a line leaves its statement open.
fn continues(line: &str) -> bool {
    line.trim_end().ends_with([
        ',', '\\', '=', '+', '|', '&', '.', '(', '[', '{', '?', ':', '<', '>',
    ])
}

/// The index of the last line of the declaration that starts at `first`, and
/// whether it ran past `cap` lines. A line ending in `:` opens an indented
/// block (Python); otherwise the declaration ends where the brackets it opened
/// close, at a `;`, or at the line that ends its statement.
pub(crate) fn decl_end(lines: &[String], first: usize, cap: usize) -> (usize, bool) {
    let Some(head) = lines.get(first) else {
        return (first, false);
    };
    let block = head.trim_end().ends_with(':');
    let base = indent_of(head);
    let (mut depth, mut braced, mut end) = (0_i32, false, first);
    for (offset, line) in lines[first..].iter().take(cap).enumerate() {
        let index = first + offset;
        if block && offset > 0 && !line.trim().is_empty() && indent_of(line) <= base {
            return (end, false);
        }
        if !line.trim().is_empty() {
            end = index;
        }
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    braced = true;
                }
                '(' | '[' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        if block {
            continue;
        }
        let text = line.trim_end();
        let next_opens = lines
            .get(index + 1)
            .is_some_and(|next| next.trim_start().starts_with('{'));
        if depth <= 0 && !next_opens && (braced || text.ends_with(';') || !continues(text)) {
            return (index, false);
        }
    }
    (end, first + cap <= lines.len())
}

/// Number the `lines` (the first one is line `first`) after taking off the
/// indentation they share, each bounded to [`MAX_LINE_CHARS`].
pub(crate) fn number_lines(first: u64, lines: &[String]) -> Vec<String> {
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|ch| ch.is_whitespace()).count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let text: String = line
                .chars()
                .skip(indent)
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            let text = text.trim_end();
            let cut = if text.chars().count() > MAX_LINE_CHARS {
                let head: String = text.chars().take(MAX_LINE_CHARS).collect();
                format!("{head}…")
            } else {
                text.to_string()
            };
            format!("{}| {cut}", first + offset as u64)
        })
        .collect()
}

/// A block of the lines `first..=last` (1-based, inclusive) of `lines`.
pub(crate) fn range_block(
    label: String,
    path: &str,
    lines: &[String],
    first: u64,
    last: u64,
) -> Option<Block> {
    let from = usize::try_from(first.checked_sub(1)?).ok()?;
    let to = usize::try_from(last.checked_sub(1)?)
        .ok()?
        .min(lines.len().checked_sub(1)?);
    if from > to {
        return None;
    }
    Some(Block::new(
        format!("{path}:{first}-{} — {label}", to + 1),
        number_lines(first, &lines[from..=to]),
    ))
}

/// The definition block of `site`: the doc comment above, the declaration to
/// its end (the graph's, or the one [`decl_end`] finds), at most
/// [`MAX_DEF_LINES`] lines of it.
pub(crate) fn def_block(lines: &[String], site: &DefSite, name: &str) -> Option<Block> {
    let start = usize::try_from(site.start.checked_sub(1)?).ok()?;
    if start >= lines.len() {
        return None;
    }
    let (found, _) = decl_end(lines, start, MAX_DEF_LINES);
    let graph = site
        .end
        .and_then(|end| usize::try_from(end.checked_sub(1)?).ok())
        .filter(|end| *end >= start);
    let wanted = graph.map_or(found, |end| end.max(found));
    let last = wanted.min(start + MAX_DEF_LINES - 1).min(lines.len() - 1);
    let top = answer::doc_above(lines, start)
        .unwrap_or(start)
        .max(start.saturating_sub(MAX_DOC_LINES));
    let mut block = range_block(
        format!("{name} definition ({})", site.kind),
        &site.path,
        lines,
        top as u64 + 1,
        last as u64 + 1,
    )?;
    if wanted > last {
        block
            .body
            .push(format!("… {name} continues past line {}", last + 1));
    }
    Some(block)
}

/// A region of `path` that stands for a match: the declaration that encloses
/// `line` (its doc comment above, at most [`MAX_DEF_LINES`] lines), else a few
/// lines around it.
pub(crate) fn match_region(lines: &[String], line: u64) -> Option<(u64, u64)> {
    let at = usize::try_from(line.checked_sub(1)?).ok()?;
    if at >= lines.len() {
        return None;
    }
    let signature = answer::enclosing_start(1, lines, line);
    let start = usize::try_from(signature.checked_sub(1)?).ok()?;
    let (end, _) = decl_end(lines, start, LONG_DECL_LINES);
    if lines
        .get(start)
        .is_some_and(|text| answer::is_signature(text))
        && signature <= line
        && at <= end
    {
        if end - start < MAX_DEF_LINES {
            let top = answer::doc_above(lines, start)
                .unwrap_or(start)
                .max(start.saturating_sub(MAX_DOC_LINES));
            return Some((top as u64 + 1, end as u64 + 1));
        }
        // A long function: the lines around the match, not its first sixty.
        return Some((
            line.saturating_sub(LONG_BEFORE).max(signature),
            (line + LONG_AFTER).min(end as u64 + 1),
        ));
    }
    let first = line.saturating_sub(MATCH_BEFORE).max(1);
    let last = (line + MATCH_AFTER).min(lines.len() as u64);
    Some((first, last))
}

/// A usage site: a line of a file that mentions the symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) path: String,
    pub(crate) line: u64,
}

/// The sites of a symbol among search rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sites {
    /// Code sites, at most [`MAX_SITES`] (or the cap given), one file at a
    /// time in the order the files were found.
    pub(crate) code: Vec<Site>,
    /// Sites in test files.
    pub(crate) tests: Vec<Site>,
    /// Code sites found before the cap.
    pub(crate) total: usize,
}

/// The usage sites among `rows`: rows whose text holds `name` as a whole
/// word (a row without text is kept), minus the definition's own lines,
/// generated and credential-shaped paths, and docs the question did not ask
/// for. Test files go to `tests`; code sites are taken one per file in turn,
/// so the first eight spread over the files that use the symbol.
pub(crate) fn pick_sites(
    rows: &[RichHit],
    name: &str,
    definition: Option<(&str, u64, u64)>,
    like: Option<&str>,
    wants_docs: bool,
    cap: usize,
) -> Sites {
    let mut seen: Vec<(&str, u64)> = Vec::new();
    let mut order: Vec<&str> = Vec::new();
    let mut by_file: HashMap<&str, Vec<u64>> = HashMap::new();
    let mut tests: Vec<Site> = Vec::new();
    for row in rows {
        if is_noise_path(&row.path)
            || row.line == 0
            || row
                .text
                .as_deref()
                .is_some_and(|text| !answer::word_in(text, name))
            || definition.is_some_and(|(path, first, last)| {
                row.path == path && (first..=last).contains(&row.line)
            })
            || seen.contains(&(row.path.as_str(), row.line))
            || (answer::is_docs_path(&row.path) && !wants_docs)
            || !like.map_or_else(
                || is_source(&row.path),
                |path| same_language(&row.path, path),
            )
        {
            continue;
        }
        seen.push((row.path.as_str(), row.line));
        if answer::is_test_path(&row.path) {
            tests.push(Site {
                path: row.path.clone(),
                line: row.line,
            });
            continue;
        }
        if !by_file.contains_key(row.path.as_str()) {
            order.push(row.path.as_str());
        }
        by_file.entry(row.path.as_str()).or_default().push(row.line);
    }
    let total = by_file.values().map(Vec::len).sum();
    for lines in by_file.values_mut() {
        lines.sort_unstable();
    }
    let mut code: Vec<Site> = Vec::new();
    let mut round = 0;
    while code.len() < cap {
        let mut took = false;
        for path in &order {
            if code.len() < cap
                && let Some(line) = by_file.get(path).and_then(|lines| lines.get(round))
            {
                code.push(Site {
                    path: (*path).to_string(),
                    line: *line,
                });
                took = true;
            }
        }
        if !took {
            break;
        }
        round += 1;
    }
    Sites { code, tests, total }
}

/// How much of its surroundings a usage block shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    /// [`SITE_BEFORE`] lines above the site and [`SITE_AFTER`] below.
    Lines,
    /// A neighbourhood: about twenty lines.
    Neighbourhood,
    /// The whole function that holds the site, when it is short.
    Function,
}

impl Detail {
    const ALL: [Self; 3] = [Self::Lines, Self::Neighbourhood, Self::Function];

    /// Lines shown above and below the site.
    const fn around(self) -> (u64, u64) {
        match self {
            Self::Lines => (SITE_BEFORE, SITE_AFTER),
            Self::Neighbourhood => (8, 12),
            Self::Function => (15, 25),
        }
    }
}

/// Lines of a function shown whole at [`Detail::Function`].
const FUNCTION_LINES: usize = 45;

/// The lines around one usage site and the function that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) funcs: Vec<String>,
    /// The site sits in test code: an inline `#[cfg(test)]` module or a
    /// function named like a test.
    pub(crate) test: bool,
}

/// The window of the site at `line` at `detail`, kept inside the function
/// that holds the line when one does, blank edges trimmed.
pub(crate) fn site_window(lines: &[String], line: u64, detail: Detail) -> Option<Window> {
    let at = usize::try_from(line.checked_sub(1)?).ok()?;
    if at >= lines.len() {
        return None;
    }
    let signature = answer::enclosing_start(1, lines, line);
    let start = usize::try_from(signature.checked_sub(1)?).ok()?;
    let function = lines
        .get(start)
        .filter(|text| answer::is_signature(text))
        .and_then(|text| answer::symbol_of_signature(text));
    let (end, _) = decl_end(lines, start, 400);
    let inside = function.is_some() && signature <= line && at <= end;
    let (before, after) = detail.around();
    let (mut first, mut last) = (
        line.saturating_sub(before).max(1),
        (line + after).min(lines.len() as u64),
    );
    if inside {
        if detail == Detail::Function && end - start < FUNCTION_LINES {
            (first, last) = (signature, end as u64 + 1);
        }
        first = first.max(signature);
        last = last.min(end as u64 + 1);
    }
    let blank = |number: u64| {
        usize::try_from(number - 1)
            .ok()
            .and_then(|index| lines.get(index))
            .is_some_and(|text| text.trim().is_empty())
    };
    while first < line && blank(first) {
        first += 1;
    }
    while last > line && blank(last) {
        last -= 1;
    }
    let test = answer::in_test_module(lines, line)
        || function.as_deref().is_some_and(answer::is_test_symbol);
    Some(Window {
        first,
        last,
        funcs: function.into_iter().collect(),
        test,
    })
}

/// Windows of one file, overlapping or touching ones joined.
pub(crate) fn merge_windows(mut windows: Vec<Window>) -> Vec<Window> {
    windows.sort_by_key(|window| window.first);
    let mut merged: Vec<Window> = Vec::new();
    for window in windows {
        match merged.last_mut() {
            Some(held) if window.first <= held.last + 1 => {
                held.last = held.last.max(window.last);
                for func in window.funcs {
                    if !held.funcs.contains(&func) {
                        held.funcs.push(func);
                    }
                }
                held.test |= window.test;
            }
            _ => merged.push(window),
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// Names a prompt in plain words points at.
// ---------------------------------------------------------------------------

static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]{3,}").expect("the identifier pattern is a literal")
});
static QUOTED_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    // One alternative per quote mark: the regex engine has no backreference.
    Regex::new(
        r#"(?:^|[\s(\[])(?:'([^'\n]{12,160}?)'|"([^"\n]{12,160}?)"|`([^`\n]{12,160}?)`)(?:$|[\s.,;:!?)\]])"#,
    )
    .expect("the quoted-phrase pattern is a literal")
});

/// Characters of a phrase segment worth a text search of its own.
const MIN_NEEDLE_CHARS: usize = 12;
/// Phrases searched.
const MAX_PHRASES: usize = 2;

/// Quoted passages of the prompt that read like a message, not a name: at
/// least two words and [`MIN_NEEDLE_CHARS`] characters. The text of an error
/// the user pasted between quotes is the best anchor a prompt can give.
pub(crate) fn quoted_phrases(typed: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in QUOTED_PHRASE.captures_iter(typed) {
        let phrase = (1..=3)
            .find_map(|group| caps.get(group))
            .map_or("", |found| found.as_str())
            .trim();
        if phrase.split_whitespace().count() >= 2 && !found.iter().any(|held| held == phrase) {
            found.push(phrase.to_string());
        }
    }
    found.truncate(MAX_PHRASES);
    found
}

/// The strings to search a phrase by, best first: the phrase, then its
/// segments between `:` `(` `)` `,` `;` (a message often ends in a value the
/// code formats in), longest first.
pub(crate) fn phrase_needles(phrase: &str) -> Vec<String> {
    let mut needles = vec![phrase.to_string()];
    let mut segments: Vec<String> = phrase
        .split([':', '(', ')', ',', ';', '{', '}'])
        .map(|segment| segment.trim().to_string())
        .filter(|segment| segment.chars().count() >= MIN_NEEDLE_CHARS && segment != phrase)
        .collect();
    segments.sort_by_key(|segment| std::cmp::Reverse(segment.len()));
    needles.extend(segments);
    needles
}

/// Identifiers in `hints` whose words overlap the prompt's `terms`: a name
/// that spells at least two of them (`SEARCH_DEFAULT_ROWS` for "default number
/// of rows a search returns") is very likely what the prompt is about. Best
/// first; an earlier hint outranks a later one; at most [`MAX_TARGETS`].
pub(crate) fn rank_identifiers(hints: &[String], terms: &[String]) -> Vec<String> {
    let wanted: HashSet<String> = terms.iter().flat_map(|term| answer::tokens(term)).collect();
    let mut scored: Vec<(i64, String)> = Vec::new();
    for (rank, hint) in hints.iter().enumerate() {
        for found in IDENTIFIER.find_iter(hint) {
            let name = found.as_str();
            let words: HashSet<String> = answer::tokens(name).into_iter().collect();
            let overlap = words.intersection(&wanted).count();
            if overlap < 2 || scored.iter().any(|(_, held)| held == name) {
                continue;
            }
            let bonus = if words.len() >= 3 { 2 } else { 0 };
            scored.push((
                i64::try_from(overlap * 10 + bonus).unwrap_or(0)
                    - i64::try_from(rank.min(9)).unwrap_or(0),
                name.to_string(),
            ));
        }
    }
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let best = scored.first().map_or(0, |(score, _)| *score / 10);
    let mut kept: Vec<String> = Vec::new();
    for (score, name) in scored {
        // Names as good as the best, then one runner-up.
        if score / 10 >= best || kept.len() < 2 {
            kept.push(name);
        }
        if kept.len() == MAX_TARGETS {
            break;
        }
    }
    kept
}

/// Whether `a` and `b` are written in the same language, by extension:
/// the dialects of one language count as one.
pub(crate) fn same_language(a: &str, b: &str) -> bool {
    let family = |path: &str| -> String {
        let extension = path
            .rsplit_once('.')
            .map_or("", |(_, ext)| ext)
            .to_ascii_lowercase();
        match extension.as_str() {
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => "js".to_string(),
            "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => "c".to_string(),
            "py" | "pyi" => "py".to_string(),
            _ => extension,
        }
    };
    family(a) == family(b)
}

/// Extensions of files that hold code a question can be about: the ones a
/// symbol is declared in, not the data and prose around them.
const CODE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cjs", "cpp", "cs", "cts", "cxx", "dart", "ex", "exs", "go", "h", "hpp", "java",
    "js", "jsx", "kt", "lua", "mjs", "mts", "php", "py", "pyi", "rb", "rs", "scala", "swift", "ts",
    "tsx", "vue", "zig",
];

/// A code file by extension.
fn is_source(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    })
}

// ---------------------------------------------------------------------------
// The builder: evidence in, a pack out.
// ---------------------------------------------------------------------------

/// A region the chain already picked for a prompt that names no symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadRegion {
    pub(crate) path: String,
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) symbol: Option<String>,
}

/// What the chain knows when it builds the pack.
pub(crate) struct Input {
    /// Identifier anchors of the prompt, in order.
    pub(crate) names: Vec<String>,
    /// Quoted passages of the prompt that read like a message.
    pub(crate) phrases: Vec<String>,
    /// Path anchors of the prompt.
    pub(crate) paths: Vec<String>,
    /// Declarations the graph found for the first name.
    pub(crate) defined: Vec<DefSite>,
    /// The graph could not answer (stale, unbuilt, outdated).
    pub(crate) graph_down: bool,
    /// Rows of the literal search of the first name, when the chain ran it.
    pub(crate) first_rows: Option<Vec<RichHit>>,
    /// Direct callers the graph reported, as `(path, line)`.
    pub(crate) callers: Vec<(String, u64)>,
    /// Ranges the chain picked from the best files of a prompt in plain words.
    pub(crate) reads: Vec<ReadRegion>,
    /// The ranges are shown beside the symbols (a prompt in plain words).
    pub(crate) regions: bool,
    /// Texts and symbols of the best files, where a name may be found.
    pub(crate) hints: Vec<String>,
    /// The words of the prompt the search weighed.
    pub(crate) terms: Vec<String>,
    /// Lines of graph facts worth keeping beside the regions (a flow).
    pub(crate) facts: Vec<String>,
    pub(crate) wants_tests: bool,
    pub(crate) wants_docs: bool,
    /// Blocks a locator found for a question about a setting or about tests,
    /// best first. They stand beside the symbols' blocks, and alone when the
    /// prompt names no symbol.
    pub(crate) locators: Vec<Block>,
    /// How much the host takes.
    pub(crate) limit: Limit,
    /// Ranges this session was sent by an earlier pack: none of them is sent
    /// again, and the room they would have taken goes to blocks it has not got.
    pub(crate) seen: Seen,
}

/// A symbol (or a region standing for one) the pack is about.
#[derive(Clone, Debug)]
struct Target {
    name: String,
    definition: Option<DefSite>,
    region: Option<ReadRegion>,
    /// Rows that may be usage sites.
    rows: Vec<RichHit>,
    /// The definition came from the text search, not the graph.
    by_text: bool,
    /// Sites shown at most.
    sites: usize,
    /// Show the declaration around the first match of the best files.
    matches: bool,
    /// A region of the best file: shown whole when the file is short.
    whole_file: bool,
}

/// File contents read once and kept, bounded by [`MAX_FILES`].
struct Files<'a> {
    evidence: &'a dyn Evidence,
    deadline: Instant,
    held: HashMap<String, Option<Vec<String>>>,
    searched: HashMap<String, Vec<RichHit>>,
}

impl<'a> Files<'a> {
    fn new(evidence: &'a dyn Evidence, deadline: Instant) -> Self {
        Self {
            evidence,
            deadline,
            held: HashMap::new(),
            searched: HashMap::new(),
        }
    }

    /// The rows of the literal search of `needle`, searched once.
    fn search(&mut self, needle: &str) -> Vec<RichHit> {
        if !self.searched.contains_key(needle) {
            let rows = if Instant::now() >= self.deadline {
                Vec::new()
            } else {
                self.evidence
                    .files_matching(needle, self.deadline)
                    .map(|found| found.hits)
                    .unwrap_or_default()
            };
            self.searched.insert(needle.to_string(), rows);
        }
        self.searched.get(needle).cloned().unwrap_or_default()
    }

    fn get(&mut self, path: &str) -> Option<&Vec<String>> {
        if !self.held.contains_key(path) {
            if self.held.len() >= MAX_FILES || Instant::now() >= self.deadline {
                return None;
            }
            let lines = self
                .evidence
                .lines_at(path, 1, MAX_FILE_LINES, self.deadline)
                .ok();
            self.held.insert(path.to_string(), lines);
        }
        self.held.get(path).and_then(Option::as_ref)
    }
}

/// The search rows for `name`: the callers the graph reported first when
/// `with_callers`, then the rows of the literal search (the chain's own when
/// it ran it for this name).
fn rows_of(
    name: &str,
    own: Option<Vec<RichHit>>,
    callers: &[(String, u64)],
    evidence: &dyn Evidence,
    deadline: Instant,
) -> (Vec<RichHit>, Vec<RichHit>) {
    let found = own
        .or_else(|| {
            evidence
                .files_matching(name, deadline)
                .ok()
                .map(|found| found.hits)
        })
        .unwrap_or_default();
    let mut rows: Vec<RichHit> = callers
        .iter()
        .map(|(path, line)| RichHit {
            path: path.clone(),
            line: *line,
            text: None,
        })
        .collect();
    rows.extend(found.iter().cloned());
    (rows, found)
}

/// Targets for one identifier: its declarations (the graph's, else the text
/// search's) ranked, with the rows that may use it.
fn name_targets(
    name: &str,
    primary: Option<&Input>,
    input: &Input,
    evidence: &dyn Evidence,
    deadline: Instant,
) -> Vec<Target> {
    let (rows, found) = rows_of(
        name,
        primary.and_then(|input| input.first_rows.clone()),
        primary.map_or(&[][..], |input| input.callers.as_slice()),
        evidence,
        deadline,
    );
    let graph: Option<Vec<DefSite>> = match primary {
        Some(input) => (!input.graph_down).then(|| input.defined.clone()),
        None => evidence
            .symbols(name, deadline)
            .ok()
            .map(|hits| hits.iter().map(DefSite::from).collect()),
    };
    let (defs, by_text) = match graph.filter(|defs| !defs.is_empty()) {
        Some(defs) => (defs, false),
        None => (text_definitions(&found, name), true),
    };
    let ranked = rank_defs(defs, &input.paths, input.wants_tests, input.wants_docs);
    if ranked.len() > AMBIGUOUS_DEFINITIONS && input.paths.is_empty() {
        return Vec::new();
    }
    let Some(best) = ranked.first().cloned() else {
        return if rows.is_empty() {
            Vec::new()
        } else {
            vec![Target {
                name: name.to_string(),
                definition: None,
                region: None,
                rows,
                by_text: false,
                sites: MAX_SITES,
                matches: true,
                whole_file: false,
            }]
        };
    };
    let demoted = |site: &DefSite| answer::demoted(&site.path, input.wants_tests, input.wants_docs);
    ranked
        .into_iter()
        .filter(|site| demoted(site) == demoted(&best))
        .take(NAME_DEFINITIONS)
        .map(|site| Target {
            name: name.to_string(),
            definition: Some(site),
            region: None,
            rows: rows.clone(),
            by_text,
            sites: MAX_SITES,
            matches: false,
            whole_file: false,
        })
        .collect()
}

/// A phrase's targets: the first needle that matches anywhere, and the
/// declaration around its first matches.
fn phrase_target(phrase: &str, evidence: &dyn Evidence, deadline: Instant) -> Option<Target> {
    phrase_needles(phrase).into_iter().find_map(|needle| {
        let rows = evidence
            .files_matching(&needle, deadline)
            .ok()?
            .hits
            .into_iter()
            .filter(|row| !is_noise_path(&row.path) && is_source(&row.path))
            .collect::<Vec<_>>();
        (!rows.is_empty()).then_some(Target {
            name: needle,
            definition: None,
            region: None,
            rows,
            by_text: false,
            sites: MAX_SITES,
            matches: true,
            whole_file: false,
        })
    })
}

/// Region targets added beside the symbols of a prompt in plain words.
const MAX_REGIONS: usize = 3;
/// A best file this short is shown whole.
const WHOLE_FILE_LINES: usize = 240;
/// Lines of one block cut from a file shown whole.
const WHOLE_CHUNK: usize = 60;
/// Most lines of a widened region.
const MAX_WIDE_LINES: u64 = 110;
/// Lines a region, a match or a short declaration is widened by at each
/// [`Detail`].
const WIDEN: [u64; 3] = [0, 10, 25];
/// A declaration this short is a value or a field: its neighbours matter.
const COMPACT_DECL_LINES: usize = 6;

/// The symbols the pack is about, with their candidate rows. The identifiers
/// the prompt names come first; a prompt that names none is searched by the
/// passages it quotes, then by the names its words spell in the best files.
/// A prompt in plain words also gets the regions the chain picked.
fn resolve_targets(input: &Input, evidence: &dyn Evidence, deadline: Instant) -> Vec<Target> {
    let alive = || Instant::now() < deadline;
    let mut targets: Vec<Target> = Vec::new();
    for (index, name) in input.names.iter().take(MAX_NAMES).enumerate() {
        if targets.len() < MAX_TARGETS && alive() {
            let primary = (index == 0).then_some(input);
            targets.extend(name_targets(name, primary, input, evidence, deadline));
        }
    }
    let mut quoted = false;
    if targets.is_empty() {
        for phrase in &input.phrases {
            if alive()
                && let Some(target) = phrase_target(phrase, evidence, deadline)
            {
                quoted = true;
                targets.push(target);
            }
        }
    }
    // Names the prompt's words spell are a lead for a prompt that gave none;
    // a passage found in the code is already one.
    if input.regions && !quoted && targets.iter().all(|target| target.matches) {
        let mut spelled = rank_identifiers(&input.hints, &input.terms);
        spelled.retain(|name| !input.names.contains(name));
        for name in spelled {
            if targets.len() < MAX_TARGETS && alive() {
                targets.extend(name_targets(&name, None, input, evidence, deadline));
            }
        }
    }
    targets.truncate(MAX_TARGETS);
    // The regions the chain picked are the lead of last resort.
    if input.regions && targets.is_empty() {
        for (index, region) in input.reads.iter().take(MAX_REGIONS).enumerate() {
            let symbol = region.symbol.clone().filter(|symbol| {
                symbol.len() >= 5
                    && answer::tokens(symbol).iter().any(|word| {
                        input
                            .terms
                            .iter()
                            .any(|term| answer::tokens(term).contains(word))
                    })
            });
            let rows = symbol
                .as_deref()
                .filter(|_| alive())
                .and_then(|symbol| evidence.files_matching(symbol, deadline).ok())
                .map(|found| found.hits)
                .unwrap_or_default();
            let stem = region
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&region.path)
                .to_string();
            targets.push(Target {
                name: symbol.unwrap_or(stem),
                definition: None,
                region: Some(region.clone()),
                rows,
                by_text: false,
                sites: REGION_SITES,
                matches: false,
                whole_file: index == 0,
            });
        }
    }
    targets
}

/// The candidate blocks of one target at one level of detail, and how many
/// code sites its search found; the test blocks (the same at every level)
/// travel apart.
#[derive(Clone, Debug, Default)]
struct Level {
    blocks: Vec<Candidate>,
    sites: usize,
}

/// The pack for `input`, or `None` when nothing is worth showing. `publish`
/// receives the pack as it stands after each stage, so a reader that stops
/// waiting still sees complete blocks.
pub(crate) fn build(
    input: &Input,
    evidence: &dyn Evidence,
    deadline: Instant,
    publish: &mut dyn FnMut(&Pack),
) -> Option<Pack> {
    let targets = resolve_targets(input, evidence, deadline);
    if targets.is_empty() && input.locators.is_empty() {
        return None;
    }
    let mut files = Files::new(evidence, deadline);
    let fallback = targets.iter().any(|target| target.by_text) && input.graph_down;
    let mut base: Vec<Candidate> = Vec::new();
    if !input.facts.is_empty() {
        base.push(Candidate {
            priority: 4,
            order: (usize::MAX, 0, 0),
            part: Part::Facts,
            block: Block::new("call graph facts".to_string(), input.facts.clone()),
        });
    }
    for (rank, block) in input
        .locators
        .iter()
        .filter(|block| !is_seen(&input.seen, block))
        .enumerate()
    {
        base.push(Candidate {
            priority: rank,
            order: (usize::MAX - 1, rank, 0),
            part: Part::Locator,
            block: block.clone(),
        });
    }
    // The blocks of each target at each level of detail, level zero first so
    // something is published early; the tests are read once.
    let mut levels: Vec<Vec<Level>> = Vec::new();
    let mut tests: Vec<Candidate> = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    let mut taken: Vec<(String, u64, u64)> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let repeats = target.region.is_none() && named.contains(&target.name.as_str());
        named.push(&target.name);
        let mut ladder: Vec<Level> = Vec::new();
        let mut shown: Vec<(String, u64, u64)> = Vec::new();
        for (step, detail) in Detail::ALL.into_iter().enumerate() {
            if repeats || (step > 0 && Instant::now() >= deadline) {
                break;
            }
            let mut context = Context {
                input,
                deadline,
                files: &mut files,
            };
            let (mut level, mut listed) = target_level(index, target, detail, &mut context);
            // A region another block already shows adds nothing, nor does one
            // this session was sent by an earlier pack.
            level.blocks.retain(|candidate| {
                (target.region.is_none() || !overlaps(&taken, &candidate.block))
                    && !is_seen(&input.seen, &candidate.block)
            });
            listed.retain(|candidate| !is_seen(&input.seen, &candidate.block));
            if step == 0 {
                tests.extend(listed);
                shown = level
                    .blocks
                    .iter()
                    .filter_map(|candidate| block_range(&candidate.block))
                    .collect();
            }
            ladder.push(level);
        }
        taken.extend(shown);
        // A line the target's own block shows is not located twice.
        base.retain(|candidate| {
            candidate.part != Part::Locator || !overlaps(&taken, &candidate.block)
        });
        if ladder.is_empty() {
            ladder.push(Level::default());
        }
        levels.push(ladder);
        let stage = select(&base, &tests, &levels, &vec![0; levels.len()]);
        if let Some((pack, _)) = assemble(input, &targets, &stage, fallback, false) {
            publish(&pack);
        }
    }
    let tests = unseen_tests(tests);
    // Raise the detail of each target, in turn, while everything still fits.
    let mut chosen = vec![0; levels.len()];
    let mut best = select(&base, &tests, &levels, &chosen);
    let fits_all = assemble(input, &targets, &best, fallback, true).is_some_and(|(_, all)| all);
    for index in 0..levels.len() {
        if !fits_all {
            break;
        }
        for step in (1..levels[index].len()).rev() {
            let mut trial = chosen.clone();
            trial[index] = step;
            let stage = select(&base, &tests, &levels, &trial);
            if assemble(input, &targets, &stage, fallback, true).is_some_and(|(_, all)| all) {
                chosen = trial;
                best = stage;
                break;
            }
        }
    }
    assemble(input, &targets, &best, fallback, true).map(|(pack, _)| pack)
}

/// `tests` with each entry (and each body) kept the first time it appears:
/// two symbols reached through the same test list it once.
fn unseen_tests(tests: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for mut candidate in tests {
        match candidate.part {
            Part::TestBody => {
                if !seen.insert(candidate.block.head.clone()) {
                    continue;
                }
            }
            _ => {
                candidate
                    .block
                    .body
                    .retain(|entry| seen.insert(entry.clone()));
                if candidate.block.body.is_empty() {
                    continue;
                }
            }
        }
        out.push(candidate);
    }
    out
}

/// The path and line range a block shows, from its head line.
pub(crate) fn block_range(block: &Block) -> Option<(String, u64, u64)> {
    let place = block.head.split(" — ").next()?;
    let (path, range) = place.rsplit_once(':')?;
    let (first, last) = range.split_once('-')?;
    Some((path.to_string(), first.parse().ok()?, last.parse().ok()?))
}

/// Whether `block` shows lines an earlier pack already sent to this session.
fn is_seen(seen: &Seen, block: &Block) -> bool {
    block_range(block).is_some_and(|(path, first, last)| seen.overlaps(&path, first, last))
}

/// Whether `block` shows lines some block already taken shows.
fn overlaps(taken: &[(String, u64, u64)], block: &Block) -> bool {
    block_range(block).is_some_and(|(path, first, last)| {
        taken
            .iter()
            .any(|(held, a, b)| *held == path && *a <= last && first <= *b)
    })
}

/// The candidates of the chosen level of each target.
fn select(
    base: &[Candidate],
    tests: &[Candidate],
    levels: &[Vec<Level>],
    chosen: &[usize],
) -> (Vec<Candidate>, usize) {
    let mut all: Vec<Candidate> = base.to_vec();
    all.extend(tests.iter().cloned());
    let mut found = 0;
    for (ladder, step) in levels.iter().zip(chosen) {
        if let Some(level) = ladder.get(*step) {
            all.extend(level.blocks.iter().cloned());
            found += level.sites;
        }
    }
    (all, found)
}

/// What building a target's blocks needs besides the target.
struct Context<'a, 'b> {
    input: &'a Input,
    deadline: Instant,
    files: &'a mut Files<'b>,
}

/// `first..=last` widened by `by` lines each way inside a file of `total`
/// lines, at most [`MAX_WIDE_LINES`] long.
pub(crate) fn widen(total: usize, first: u64, last: u64, by: u64) -> (u64, u64) {
    let first = first.saturating_sub(by).max(1);
    let last = (last + by).min(total as u64).max(first);
    (first, last.min(first + MAX_WIDE_LINES - 1))
}

/// The blocks of a file shown whole: [`WHOLE_CHUNK`] lines each.
fn whole_file_blocks(index: usize, path: &str, lines: &[String]) -> Vec<Candidate> {
    (0..lines.len())
        .step_by(WHOLE_CHUNK)
        .enumerate()
        .filter_map(|(chunk, from)| {
            let first = from as u64 + 1;
            let last = (from + WHOLE_CHUNK).min(lines.len()) as u64;
            let block = range_block(
                "whole file (best match)".to_string(),
                path,
                lines,
                first,
                last,
            )?;
            Some(Candidate {
                priority: if chunk == 0 {
                    MAX_TARGETS
                } else {
                    MAX_TARGETS + 4 + chunk * MAX_TARGETS
                },
                order: (index, 0, first),
                part: Part::Definition,
                block,
            })
        })
        .collect()
}

/// The blocks of `target` at `detail`: its definition (or region, or match),
/// its usage windows; and, apart, its tests.
fn target_level(
    index: usize,
    target: &Target,
    detail: Detail,
    context: &mut Context,
) -> (Level, Vec<Candidate>) {
    let step = Detail::ALL
        .iter()
        .position(|held| *held == detail)
        .unwrap_or(0);
    let mut level = Level::default();
    level
        .blocks
        .extend(definition_candidates(index, target, step, context.files));
    let (usage, sites, listed) = usage_candidates(index, target, detail, context);
    // A usage the definition's own block already shows is not shown twice.
    let shown: Vec<(String, u64, u64)> = level
        .blocks
        .iter()
        .filter_map(|candidate| block_range(&candidate.block))
        .collect();
    level.blocks.extend(
        usage
            .into_iter()
            .filter(|candidate| !overlaps(&shown, &candidate.block)),
    );
    level.sites = sites;
    (level, listed)
}

/// The definition, region or match blocks of a target at detail `step`.
fn definition_candidates(
    index: usize,
    target: &Target,
    step: usize,
    files: &mut Files,
) -> Vec<Candidate> {
    let by = WIDEN[step];
    if let Some(site) = &target.definition {
        let Some(lines) = files.get(&site.path) else {
            return Vec::new();
        };
        let Some(block) = def_block(lines, site, &target.name) else {
            return Vec::new();
        };
        let mut out = vec![Candidate {
            priority: index,
            order: (index, 0, site.start),
            part: Part::Definition,
            block: block.clone(),
        }];
        // A value or a field sits among its siblings: the neighbours say
        // what the group is.
        if by > 0
            && matches!(
                site.kind.as_str(),
                "const" | "value" | "variant" | "field" | "variable"
            )
            && let Some((first, last)) = block_range(&block).map(|(_, a, b)| (a, b))
            && last - first < COMPACT_DECL_LINES as u64
        {
            let (from, to) = widen(lines.len(), first, last, by);
            if let Some(wide) = range_block(
                format!(
                    "{} definition ({}) with its neighbours",
                    target.name, site.kind
                ),
                &site.path,
                lines,
                from,
                to,
            ) {
                out[0].block = wide;
            }
        }
        return out;
    }
    if let Some(region) = &target.region {
        let Some(lines) = files.get(&region.path) else {
            return Vec::new();
        };
        if target.whole_file && lines.len() <= WHOLE_FILE_LINES {
            return whole_file_blocks(index, &region.path, lines);
        }
        let (first, last) = widen(lines.len(), region.first, region.last, by);
        return range_block(
            format!("{} region", target.name),
            &region.path,
            lines,
            first,
            last,
        )
        .map(|block| {
            vec![Candidate {
                priority: MAX_TARGETS + index,
                order: (index, 0, first),
                part: Part::Definition,
                block,
            }]
        })
        .unwrap_or_default();
    }
    if target.matches {
        return match_candidates(index, target, by, files);
    }
    Vec::new()
}

/// For a symbol or passage no declaration was found for: the declaration
/// around the first match in each of the two best code files.
fn match_candidates(index: usize, target: &Target, by: u64, files: &mut Files) -> Vec<Candidate> {
    let sites = pick_sites(&target.rows, &target.name, None, None, false, 2 * MAX_FILES);
    let mut firsts: Vec<Site> = Vec::new();
    for site in sites.code {
        if firsts.len() < 2 && !firsts.iter().any(|held| held.path == site.path) {
            firsts.push(site);
        }
    }
    let mut out = Vec::new();
    for site in firsts {
        let Some(lines) = files.get(&site.path) else {
            continue;
        };
        let Some((first, last)) = match_region(lines, site.line) else {
            continue;
        };
        let (first, last) = widen(lines.len(), first, last, by);
        let Some(block) = range_block(
            format!("{} match", target.name),
            &site.path,
            lines,
            first,
            last,
        ) else {
            continue;
        };
        out.push(Candidate {
            priority: index + out.len(),
            order: (index, 0, first),
            part: Part::Definition,
            block,
        });
    }
    out
}

/// Usage blocks of a target at `detail`, how many code sites the search
/// found, and the test blocks (the same at every detail).
fn usage_candidates(
    index: usize,
    target: &Target,
    detail: Detail,
    context: &mut Context,
) -> (Vec<Candidate>, usize, Vec<Candidate>) {
    let definition = target.definition.as_ref().map(|site| {
        let end = site.end.unwrap_or(site.start);
        (site.path.as_str(), site.start, end.max(site.start))
    });
    let like = target
        .definition
        .as_ref()
        .map(|site| site.path.as_str())
        .or_else(|| target.region.as_ref().map(|region| region.path.as_str()));
    // Sites the session was sent already do not count against the cap: with
    // some sent, twice as many are weighed, and the first `target.sites` that
    // are new are shown.
    let ask = if context.input.seen.is_empty() {
        target.sites
    } else {
        target.sites.saturating_mul(2)
    };
    let sites = pick_sites(
        &target.rows,
        &target.name,
        definition,
        like,
        context.input.wants_docs,
        ask,
    );
    let mut out: Vec<Candidate> = Vec::new();
    let mut inline_tests: Vec<String> = Vec::new();
    let mut funcs: Vec<String> = Vec::new();
    let mut by_file: Vec<(String, Vec<(usize, Window)>)> = Vec::new();
    let mut fresh = 0;
    for (rank, site) in sites.code.iter().enumerate() {
        if fresh >= target.sites {
            break;
        }
        let Some(lines) = context.files.get(&site.path) else {
            continue;
        };
        let Some(window) = site_window(lines, site.line, detail) else {
            continue;
        };
        if context
            .input
            .seen
            .overlaps(&site.path, window.first, window.last)
        {
            continue;
        }
        fresh += 1;
        if window.test {
            if !inline_tests.contains(&site.path) {
                inline_tests.push(site.path.clone());
            }
            continue;
        }
        for func in &window.funcs {
            if func.len() >= 4 && *func != target.name && !funcs.contains(func) {
                funcs.push(func.clone());
            }
        }
        match by_file.iter_mut().find(|(path, _)| *path == site.path) {
            Some((_, windows)) => windows.push((rank, window)),
            None => by_file.push((site.path.clone(), vec![(rank, window)])),
        }
    }
    for (path, windows) in by_file {
        let rank_of = |first: u64, last: u64| {
            windows
                .iter()
                .filter(|(_, w)| w.first <= last && w.last >= first)
                .map(|(rank, _)| *rank)
                .min()
                .unwrap_or(0)
        };
        let Some(lines) = context.files.get(&path) else {
            continue;
        };
        let merged = merge_windows(windows.iter().map(|(_, w)| w.clone()).collect());
        for window in merged {
            let funcs = if window.funcs.is_empty() {
                String::new()
            } else {
                format!(" in {}", window.funcs.join(", "))
            };
            let Some(block) = range_block(
                format!("{} usage{funcs}", target.name),
                &path,
                lines,
                window.first,
                window.last,
            ) else {
                continue;
            };
            out.push(Candidate {
                priority: MAX_TARGETS
                    + 4
                    + rank_of(window.first, window.last) * MAX_TARGETS
                    + index,
                order: (index, 1, window.first),
                part: Part::Usage,
                block,
            });
        }
    }
    if target
        .definition
        .as_ref()
        .is_some_and(|site| matches!(site.kind.as_str(), "function" | "method"))
    {
        out.extend(caller_candidates(
            index, target, &funcs, like, detail, context,
        ));
    }
    let listed = if detail == Detail::Lines {
        test_candidates(index, target, &sites, &inline_tests, &funcs, context)
    } else {
        Vec::new()
    };
    (out, sites.total, listed)
}

/// Functions whose callers are followed (the functions that use the target).
const HOP_FUNCS: usize = 3;
/// Call sites shown per such function.
const HOP_SITES: usize = 2;
/// A name found on more rows than this is too common to follow.
const COMMON_ROWS: usize = 40;

/// The call sites of the functions that use the target: what a change to the
/// target reaches one step further out.
fn caller_candidates(
    index: usize,
    target: &Target,
    funcs: &[String],
    like: Option<&str>,
    detail: Detail,
    context: &mut Context,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for func in funcs.iter().take(HOP_FUNCS) {
        let rows = context.files.search(func);
        if rows.len() > COMMON_ROWS {
            continue;
        }
        let sites = pick_sites(
            &rows,
            func,
            None,
            like,
            context.input.wants_docs,
            HOP_SITES + 1,
        );
        for (rank, site) in sites.code.iter().enumerate() {
            let Some(lines) = context.files.get(&site.path) else {
                continue;
            };
            let Some(window) = site_window(lines, site.line, detail) else {
                continue;
            };
            // The function's own declaration is not a call.
            if window.test || window.funcs.iter().any(|held| held == func) {
                continue;
            }
            if context
                .input
                .seen
                .overlaps(&site.path, window.first, window.last)
            {
                continue;
            }
            let caller = if window.funcs.is_empty() {
                String::new()
            } else {
                format!(" in {}", window.funcs.join(", "))
            };
            let Some(block) = range_block(
                format!("{func} usage{caller} (reaches {})", target.name),
                &site.path,
                lines,
                window.first,
                window.last,
            ) else {
                continue;
            };
            out.push(Candidate {
                priority: MAX_TARGETS + 4 + (MAX_SITES + rank) * MAX_TARGETS + index,
                order: (index, 1, window.first),
                part: Part::Usage,
                block,
            });
        }
    }
    out
}

/// A test entry `path:line name — assertion` split into its parts.
pub(crate) fn parse_test_entry(entry: &str) -> Option<(&str, u64, &str)> {
    let (place, rest) = entry.split_once(' ')?;
    let (path, line) = place.rsplit_once(':')?;
    let name = rest.split(" — ").next()?;
    Some((path, line.parse().ok()?, name))
}

/// Lines of a test shown under its name.
const TEST_BODY_LINES: usize = 18;
/// Test bodies shown per target.
const TEST_BODIES: usize = 3;

/// The tests of a target: the test files among its rows (and the files whose
/// usage sat in test code) listed by name with their first assertion; the
/// tests that reach it through the functions that use it, listed the same
/// way; and the first few bodies, when room is left.
fn test_candidates(
    index: usize,
    target: &Target,
    sites: &Sites,
    inline_tests: &[String],
    funcs: &[String],
    context: &mut Context,
) -> Vec<Candidate> {
    let mut paths: Vec<String> = Vec::new();
    for site in &sites.tests {
        if !paths.contains(&site.path) && paths.len() < 3 {
            paths.push(site.path.clone());
        }
    }
    for path in inline_tests {
        if paths.len() < 3 && !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    let mut direct: Vec<String> = Vec::new();
    for path in &paths {
        if let Some(lines) = context.files.get(path) {
            let room = MAX_TESTS.saturating_sub(direct.len());
            direct.extend(answer::tests_excerpt(path, lines, &target.name, room));
        }
    }
    // Hop two: tests that call a function that calls the target.
    let mut through: Vec<String> = Vec::new();
    for func in funcs.iter().take(3) {
        if Instant::now() >= context.deadline {
            break;
        }
        let found = context.files.search(func);
        if found.len() > COMMON_ROWS {
            continue;
        }
        let mut test_paths: Vec<String> = Vec::new();
        for hit in found {
            if answer::is_test_path(&hit.path)
                && !is_noise_path(&hit.path)
                && !test_paths.contains(&hit.path)
                && test_paths.len() < 2
            {
                test_paths.push(hit.path);
            }
        }
        for path in test_paths {
            if let Some(lines) = context.files.get(&path) {
                through.extend(answer::tests_excerpt(&path, lines, func, 2));
            }
        }
    }
    through.truncate(MAX_TESTS);
    let mut out = Vec::new();
    if !direct.is_empty() {
        out.push(Candidate {
            priority: MAX_TARGETS + index,
            order: (index, 2, 0),
            part: Part::Tests,
            block: Block::new(format!("tests of {}", target.name), direct.clone()),
        });
    }
    if !through.is_empty() {
        out.push(Candidate {
            priority: MAX_TARGETS + MAX_TARGETS + index,
            order: (index, 2, 1),
            part: Part::Tests,
            block: Block::new(
                format!("tests reaching {} through its callers", target.name),
                through.clone(),
            ),
        });
    }
    for (rank, entry) in direct.iter().chain(&through).take(TEST_BODIES).enumerate() {
        let Some((path, line, name)) = parse_test_entry(entry) else {
            continue;
        };
        let Some(lines) = context.files.get(path) else {
            continue;
        };
        let Ok(start) = usize::try_from(line.saturating_sub(1)) else {
            continue;
        };
        let (end, _) = decl_end(lines, start, TEST_BODY_LINES);
        if let Some(block) = range_block(
            format!("{name} (a test of {})", target.name),
            path,
            lines,
            line,
            end as u64 + 1,
        ) {
            out.push(Candidate {
                priority: 40 + rank * MAX_TARGETS + index,
                order: (index, 3, line),
                part: Part::TestBody,
                block,
            });
        }
    }
    out
}

/// The pack made of the candidates that fit the parts, highest priority
/// first, shown in display order; and whether every candidate fit.
fn assemble(
    input: &Input,
    targets: &[Target],
    (candidates, found_sites): &(Vec<Candidate>, usize),
    fallback: bool,
    complete: bool,
) -> Option<(Pack, bool)> {
    if candidates.is_empty() {
        return None;
    }
    let mut pack = Pack {
        searched: String::new(),
        blocks: Vec::new(),
        complete,
        // A locator's block is found by the prompt's own words.
        anchored: targets.iter().any(|target| target.region.is_none())
            || !input.locators.is_empty(),
        targets: targets.len(),
        definitions: 0,
        locators: 0,
        fallback,
    };
    let mut ranked: Vec<&Candidate> = candidates.iter().collect();
    ranked.sort_by_key(|candidate| candidate.priority);
    let mut chosen: Vec<&Candidate> = Vec::new();
    let mut all = true;
    for candidate in ranked {
        chosen.push(candidate);
        chosen.sort_by_key(|held| held.order);
        pack.blocks = chosen.iter().map(|held| held.block.clone()).collect();
        pack.searched = header(input, targets, &chosen, *found_sites, fallback);
        if !fits(&pack.text(), input.limit) {
            chosen.retain(|held| !std::ptr::eq(*held, candidate));
            all = false;
        }
    }
    pack.definitions = chosen
        .iter()
        .filter(|held| held.part == Part::Definition)
        .count();
    pack.locators = chosen
        .iter()
        .filter(|held| held.part == Part::Locator)
        .count();
    pack.blocks = chosen.iter().map(|held| held.block.clone()).collect();
    pack.searched = header(input, targets, &chosen, *found_sites, fallback);
    (!pack.blocks.is_empty()).then_some((pack, all))
}

/// The line that says what was searched and what the blocks hold.
fn header(
    input: &Input,
    targets: &[Target],
    chosen: &[&Candidate],
    found_sites: usize,
    fallback: bool,
) -> String {
    let mut names: Vec<&str> = Vec::new();
    for target in targets {
        if !names.contains(&target.name.as_str()) {
            names.push(&target.name);
        }
    }
    let count = |wanted: Part| chosen.iter().filter(|held| held.part == wanted).count();
    let tests: usize = chosen
        .iter()
        .filter(|held| held.part == Part::Tests)
        .map(|held| held.block.body.len())
        .sum();
    let source = if fallback {
        "text index (graph stale: text fallback)"
    } else if input.graph_down {
        "text index"
    } else {
        "symbol graph and text index"
    };
    let located = match count(Part::Locator) {
        0 => String::new(),
        blocks => format!(", {blocks} located block(s)"),
    };
    let searched = if names.is_empty() {
        "the words of the question".to_string()
    } else {
        names.join(", ")
    };
    format!(
        "region pack — searched {searched} in the {source}: {} definition(s), {} usage block(s) of {found_sites} code site(s) found, {tests} test(s) listed{located}. Lines are exact source, numbered.",
        count(Part::Definition),
        count(Part::Usage),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_brief::chain::{CallerHit, Found};
    use std::time::Duration;

    fn lines(source: &[&str]) -> Vec<String> {
        source.iter().map(ToString::to_string).collect()
    }

    fn row(path: &str, line: u64, text: &str) -> RichHit {
        RichHit {
            path: path.to_string(),
            line,
            text: Some(text.to_string()),
        }
    }

    fn site(path: &str, start: u64, end: Option<u64>, kind: &str) -> DefSite {
        DefSite {
            path: path.to_string(),
            start,
            end,
            kind: kind.to_string(),
        }
    }

    /// A constant, a function that uses it, and an inline test.
    fn api_source() -> Vec<String> {
        lines(&[
            "//! The wire contract.",
            "use std::cmp::min;",
            "",
            "/// Rows a search returns when the request names no limit.",
            "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            "/// The most rows a search may return.",
            "pub const SEARCH_MAX_ROWS: usize = 10_000;",
            "",
            "pub fn search(limit: Option<usize>) -> usize {",
            "    let wanted = limit.unwrap_or(SEARCH_DEFAULT_ROWS);",
            "    min(wanted, SEARCH_MAX_ROWS)",
            "}",
            "",
            "#[cfg(test)]",
            "mod tests {",
            "    #[test]",
            "    fn default_is_one_hundred() {",
            "        assert_eq!(super::SEARCH_DEFAULT_ROWS, 100);",
            "    }",
            "}",
        ])
    }

    // ---- parts -----------------------------------------------------------

    #[test]
    fn limit_for_should_follow_the_switch_the_override_and_the_setting() {
        assert_eq!(limit_for(None, None, false, 3), Limit::of(3));
        assert_eq!(limit_for(None, None, false, 1), Limit::of(1));
        assert_eq!(limit_for(None, None, false, 8), Limit::of(8));
        assert_eq!(limit_for(None, None, false, 0), Limit::of(1));
        assert_eq!(limit_for(None, None, false, 200), Limit::of(MAX_PARTS));
        assert_eq!(limit_for(Some("1"), Some("2"), false, 3), Limit::of(2));
        assert_eq!(limit_for(None, Some("0"), false, 3), Limit::of(1));
        assert_eq!(limit_for(None, Some("99"), false, 3), Limit::of(MAX_PARTS));
        assert_eq!(limit_for(None, Some("two"), false, 3), Limit::of(3));
        for off in ["0", "false", "off"] {
            let limit = limit_for(Some(off), Some("3"), true, 3);
            assert_eq!(limit, Limit::default(), "{off}");
            assert!(!limit.on(), "{off}");
        }
        assert!(Limit::of(1).on());
    }

    #[test]
    fn the_brief_command_should_stay_under_the_size_pi_and_opencode_accept() {
        let command = Limit {
            parts: 1,
            chars: CLI_CHARS,
        };
        assert_eq!(limit_for(None, None, true, 1), command);
        assert_eq!(limit_for(None, None, true, 0), command);
        assert_eq!(command.chars, 3_900);
        // `pixel brief --parts N`: the caller takes N hook-sized parts.
        assert_eq!(limit_for(None, None, true, 3), Limit::of(3));
        assert_eq!(limit_for(None, None, true, 2), Limit::of(2));
        // An explicit part count is the host saying what it takes.
        assert_eq!(limit_for(None, Some("2"), true, 3), Limit::of(2));
        assert_eq!(limit_for(Some("off"), None, true, 3), Limit::default());
    }

    #[test]
    fn a_block_should_cut_its_body_at_a_line_when_it_outgrows_the_cap() {
        let body: Vec<String> = (1..=400)
            .map(|n| format!("{n}| {}", "x".repeat(60)))
            .collect();
        let block = Block::new("a.rs:1-400 — big".to_string(), body);
        assert!(
            block.text().len() <= MAX_BLOCK_CHARS + 40,
            "{}",
            block.text().len()
        );
        assert_eq!(
            block.body.last().map(String::as_str),
            Some("… (block cut to fit)")
        );
        let kept = block.body.len() - 1;
        assert_eq!(block.body[kept - 1], format!("{kept}| {}", "x".repeat(60)));
        let small = Block::new(
            "a.rs:1-2 — small".to_string(),
            vec!["1| a".into(), "2| b".into()],
        );
        assert_eq!(small.text(), "### a.rs:1-2 — small\n1| a\n2| b");
    }

    fn pack_text(blocks: usize, lines_per_block: usize) -> String {
        let mut text = format!("{BRIEF_TAG}\nregion pack — searched x");
        for block in 0..blocks {
            text.push_str(&format!("\n### f{block}.rs:1-{lines_per_block} — b{block}"));
            for line in 1..=lines_per_block {
                text.push_str(&format!("\n{line}| {}", "y".repeat(40)));
            }
        }
        text.push('\n');
        text.push_str(DIRECTIVE);
        text
    }

    #[test]
    fn split_parts_should_keep_a_short_text_whole() {
        let text = pack_text(2, 3);
        assert_eq!(split_parts(&text, PART_CHARS), vec![text]);
    }

    #[test]
    fn split_parts_should_cut_only_before_a_block_and_stay_under_the_cap() {
        let text = pack_text(6, 20);
        let parts = split_parts(&text, 2_000);
        assert!(parts.len() >= 3, "{}", parts.len());
        for (index, part) in parts.iter().enumerate() {
            assert!(part.len() <= 2_000, "part {index}: {}", part.len());
            if index > 0 {
                assert!(
                    part.starts_with(&format!(
                        "{BRIEF_TAG} region pack, part {}/{}\n### ",
                        index + 1,
                        parts.len()
                    )),
                    "{part}"
                );
            }
        }
        // Nothing lost, nothing repeated: the parts, minus their openers,
        // are the text.
        let rejoined: Vec<&str> = parts
            .iter()
            .enumerate()
            .flat_map(|(index, part)| part.split('\n').skip(usize::from(index > 0)))
            .collect();
        assert_eq!(rejoined.join("\n"), text);
        assert!(parts.last().unwrap().ends_with(DIRECTIVE));
    }

    #[test]
    fn split_parts_should_send_the_directive_alone_when_the_last_block_fills_a_part() {
        let text = pack_text(1, 40);
        let limit = text.len() - DIRECTIVE.len() - 1;
        let parts = split_parts(&text, limit);
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].contains(DIRECTIVE));
        assert_eq!(
            parts[1],
            format!("{BRIEF_TAG} region pack, part 2/2\n{DIRECTIVE}")
        );
        // At exactly the length of the whole text it stays with its block.
        assert_eq!(split_parts(&text, text.len()).len(), 1);
    }

    #[test]
    fn split_parts_should_cut_a_block_longer_than_a_part_at_a_line() {
        let text = pack_text(1, 100);
        let parts = split_parts(&text, 1_500);
        assert!(parts.len() >= 3);
        assert!(parts.iter().all(|part| part.len() <= 1_500));
        let numbered: usize = parts
            .iter()
            .map(|part| {
                part.split('\n')
                    .filter(|line| line.contains("| yyyy"))
                    .count()
            })
            .sum();
        assert_eq!(numbered, 100);
    }

    // ---- definitions -----------------------------------------------------

    #[test]
    fn classify_definition_should_name_a_declaration_in_each_language() {
        let cases = [
            (
                "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
                "SEARCH_DEFAULT_ROWS",
                Some("value"),
            ),
            (
                "pub(crate) fn search_rows(data: &Value) -> RichFound {",
                "search_rows",
                Some("function"),
            ),
            ("pub struct Pack {", "Pack", Some("type")),
            ("def merge_setting(", "merge_setting", Some("function")),
            (
                "class SessionRedirectMixin:",
                "SessionRedirectMixin",
                Some("type"),
            ),
            (
                "export const handleError = (e) => {",
                "handleError",
                Some("value"),
            ),
            (
                "export async function handleError(e) {",
                "handleError",
                Some("function"),
            ),
            (
                "func (s *Server) Serve(ctx context.Context) error {",
                "Serve",
                Some("function"),
            ),
            ("interface Props {", "Props", Some("type")),
            ("MAX_REDIRECTS = 30", "MAX_REDIRECTS", Some("value")),
            (
                "DEFAULT_REDIRECT_LIMIT: int = 30",
                "DEFAULT_REDIRECT_LIMIT",
                Some("value"),
            ),
            // Uses are not declarations.
            (
                "let wanted = limit.unwrap_or(SEARCH_DEFAULT_ROWS);",
                "SEARCH_DEFAULT_ROWS",
                None,
            ),
            (
                "    .unwrap_or(SEARCH_DEFAULT_ROWS)",
                "SEARCH_DEFAULT_ROWS",
                None,
            ),
            ("if MAX_REDIRECTS == 30 {", "MAX_REDIRECTS", None),
            ("return merge_setting(a, b)", "merge_setting", None),
            // A longer name is not the name.
            (
                "pub const SEARCH_DEFAULT_ROWS_X: usize = 1;",
                "SEARCH_DEFAULT_ROWS",
                None,
            ),
        ];
        for (text, name, kind) in cases {
            assert_eq!(classify_definition(text, name), kind, "{text}");
        }
    }

    #[test]
    fn text_definitions_should_prefer_declarations_over_assignments_and_skip_noise() {
        let rows = vec![
            row("src/a.py", 10, "MAX_REDIRECTS = 30"),
            row("src/a.py", 20, "    if MAX_REDIRECTS > 3:"),
            row("src/b.rs", 5, "pub const MAX_REDIRECTS: u32 = 30;"),
            row("dist/c.js", 7, "const MAX_REDIRECTS = 30"),
            row("src/d.rs", 0, "pub const MAX_REDIRECTS: u32 = 31;"),
        ];
        let found = text_definitions(&rows, "MAX_REDIRECTS");
        assert_eq!(found, vec![site("src/b.rs", 5, None, "value")]);
        // With no declaration, the assignments stand in.
        let found = text_definitions(&rows[..2], "MAX_REDIRECTS");
        assert_eq!(found, vec![site("src/a.py", 10, None, "value")]);
    }

    #[test]
    fn rank_defs_should_put_the_named_file_first_then_code_then_kind() {
        let sites = vec![
            site("tests/t.rs", 1, None, "function"),
            site("src/field.rs", 1, None, "field"),
            site("src/b.rs", 1, None, "function"),
            site("src/a.rs", 1, None, "function"),
        ];
        let ranked = rank_defs(sites.clone(), &[], false, false);
        let paths: Vec<&str> = ranked.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/b.rs", "src/a.rs", "src/field.rs", "tests/t.rs"]
        );
        let ranked = rank_defs(sites.clone(), &["src/a.rs".to_string()], false, false);
        assert_eq!(ranked[0].path, "src/a.rs");
        let ranked = rank_defs(sites, &["tests/t.rs".to_string()], false, false);
        assert_eq!(ranked[0].path, "tests/t.rs");
    }

    #[test]
    fn rank_defs_should_not_demote_tests_when_the_prompt_asks_for_them() {
        let sites = vec![
            site("src/b.rs", 1, None, "field"),
            site("tests/t.rs", 1, None, "function"),
        ];
        let ranked = rank_defs(sites, &[], true, false);
        assert_eq!(ranked[0].path, "tests/t.rs");
    }

    #[test]
    fn decl_end_should_find_the_end_of_each_shape_of_declaration() {
        let rust = lines(&[
            "pub fn go(",
            "    a: u32,",
            ") -> u32 {",
            "    a + 1",
            "}",
            "const X: u32 = 5;",
            "const Y: [u32; 2] = [",
            "    1,",
            "    2,",
            "];",
            "struct Unit;",
        ]);
        assert_eq!(decl_end(&rust, 0, 60), (4, false));
        assert_eq!(decl_end(&rust, 5, 60), (5, false));
        assert_eq!(decl_end(&rust, 6, 60), (9, false));
        assert_eq!(decl_end(&rust, 10, 60), (10, false));
        let python = lines(&[
            "def merge(a, b):",
            "    if a is None:",
            "        return b",
            "",
            "    return a",
            "",
            "LIMITS = [",
            "    1,",
            "]",
            "MAX = 30",
            "class A:",
            "    pass",
        ]);
        assert_eq!(decl_end(&python, 0, 60), (4, false));
        assert_eq!(decl_end(&python, 6, 60), (8, false));
        assert_eq!(decl_end(&python, 9, 60), (9, false));
        assert_eq!(decl_end(&python, 10, 60), (11, false));
        let allman = lines(&["fn go()", "{", "    1", "}", "fn next() {}"]);
        assert_eq!(decl_end(&allman, 0, 60), (3, false));
        let arrow = lines(&[
            "export const f = (a) => {",
            "  return a;",
            "};",
            "const g = 1",
        ]);
        assert_eq!(decl_end(&arrow, 0, 60), (2, false));
        assert_eq!(decl_end(&arrow, 3, 60), (3, false));
    }

    #[test]
    fn decl_end_should_stop_at_the_cap_and_say_so() {
        let mut long = lines(&["fn long() {"]);
        long.extend((0..100).map(|n| format!("    step({n});")));
        long.push("}".to_string());
        assert_eq!(decl_end(&long, 0, 10), (9, true));
        assert_eq!(decl_end(&long, 0, 200), (101, false));
        assert_eq!(decl_end(&long, 500, 10), (500, false));
    }

    #[test]
    fn number_lines_should_take_off_the_shared_indent_and_cut_a_long_line() {
        let source = lines(&["    fn a() {", "", "        body();", "    }"]);
        assert_eq!(
            number_lines(10, &source),
            ["10| fn a() {", "11| ", "12|     body();", "13| }"]
        );
        let wide = lines(&[&"w".repeat(MAX_LINE_CHARS + 20), "tab\there"]);
        let numbered = number_lines(1, &wide);
        assert_eq!(numbered[0], format!("1| {}…", "w".repeat(MAX_LINE_CHARS)));
        assert_eq!(numbered[1], "2| tab here");
    }

    #[test]
    fn def_block_should_carry_the_doc_comment_and_the_whole_value() {
        let source = api_source();
        let block = def_block(
            &source,
            &site("api.rs", 5, Some(5), "const"),
            "SEARCH_DEFAULT_ROWS",
        )
        .unwrap();
        assert_eq!(
            block.head,
            "api.rs:4-5 — SEARCH_DEFAULT_ROWS definition (const)"
        );
        assert_eq!(
            block.body,
            [
                "4| /// Rows a search returns when the request names no limit.",
                "5| pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            ]
        );
    }

    #[test]
    fn def_block_should_take_the_function_to_its_closing_brace() {
        let source = api_source();
        let block = def_block(&source, &site("api.rs", 9, None, "function"), "search").unwrap();
        assert_eq!(block.head, "api.rs:9-12 — search definition (function)");
        assert_eq!(block.body.len(), 4);
        assert_eq!(block.body[3], "12| }");
        // The graph's end wins when it reaches further than the scan.
        let block = def_block(&source, &site("api.rs", 9, Some(13), "function"), "search").unwrap();
        assert_eq!(block.head, "api.rs:9-13 — search definition (function)");
    }

    #[test]
    fn def_block_should_cut_a_definition_at_sixty_lines_and_say_where_it_goes_on() {
        let mut source = lines(&["pub fn long() {"]);
        source.extend((1..=100).map(|n| format!("    step({n});")));
        source.push("}".to_string());
        let block = def_block(&source, &site("a.rs", 1, Some(102), "function"), "long").unwrap();
        assert_eq!(block.head, "a.rs:1-60 — long definition (function)");
        assert_eq!(block.body.len(), 61);
        assert_eq!(block.body[59], "60|     step(59);");
        assert_eq!(block.body[60], "… long continues past line 60");
    }

    #[test]
    fn def_block_should_keep_at_most_six_lines_of_doc_and_refuse_a_missing_line() {
        let mut source: Vec<String> = (1..=10).map(|n| format!("/// doc {n}")).collect();
        source.push("pub fn documented() {}".to_string());
        let block = def_block(&source, &site("a.rs", 11, None, "function"), "documented").unwrap();
        assert_eq!(block.head, "a.rs:5-11 — documented definition (function)");
        assert!(def_block(&source, &site("a.rs", 99, None, "function"), "x").is_none());
        assert!(def_block(&source, &site("a.rs", 0, None, "function"), "x").is_none());
    }

    #[test]
    fn match_region_should_take_the_enclosing_declaration_or_a_window() {
        let source = api_source();
        assert_eq!(match_region(&source, 10), Some((9, 12)));
        let mut loose = lines(&["x = 1"]);
        loose.extend((2..=40).map(|n| format!("y{n} = {n}")));
        assert_eq!(match_region(&loose, 20), Some((14, 34)));
        assert_eq!(match_region(&loose, 2), Some((1, 16)));
        assert_eq!(match_region(&loose, 40), Some((34, 40)));
        assert_eq!(match_region(&loose, 41), None);
        assert_eq!(match_region(&loose, 0), None);
    }

    #[test]
    fn match_region_should_show_the_lines_around_a_match_in_a_long_function() {
        let mut long = lines(&["fn long() {"]);
        long.extend((1..=100).map(|n| format!("    step({n});")));
        long.push("}".to_string());
        assert_eq!(match_region(&long, 60), Some((40, 90)));
        assert_eq!(match_region(&long, 5), Some((1, 35)));
        assert_eq!(match_region(&long, 100), Some((80, 102)));
        // After the function, no signature encloses the match: a window.
        long.extend(lines(&["", "let after = 1;"]));
        assert_eq!(match_region(&long, 104), Some((98, 104)));
    }

    // ---- sites -----------------------------------------------------------

    #[test]
    fn pick_sites_should_keep_whole_word_code_rows_and_split_off_tests() {
        let rows = vec![
            row("src/a.rs", 30, "let n = LIMIT + 1;"),
            row("src/a.rs", 5, "pub const LIMIT: u32 = 3;"),
            row("src/a.rs", 31, "let m = LIMIT_X;"),
            row("tests/a_test.rs", 9, "assert_eq!(LIMIT, 3);"),
            row("docs/guide.md", 2, "LIMIT is three"),
            row("dist/out.js", 3, "var LIMIT = 3"),
            row("src/b.rs", 8, "use LIMIT;"),
            row("src/b.rs", 8, "use LIMIT;"),
        ];
        let sites = pick_sites(&rows, "LIMIT", Some(("src/a.rs", 5, 5)), None, false, 8);
        assert_eq!(
            sites.code,
            [
                Site {
                    path: "src/a.rs".into(),
                    line: 30
                },
                Site {
                    path: "src/b.rs".into(),
                    line: 8
                },
            ]
        );
        assert_eq!(
            sites.tests,
            [Site {
                path: "tests/a_test.rs".into(),
                line: 9
            }]
        );
        assert_eq!(sites.total, 2);
        // Docs come back when the question asks for them.
        let with_docs = pick_sites(&rows, "LIMIT", Some(("src/a.rs", 5, 5)), None, true, 8);
        assert_eq!(with_docs.code.len(), 2);
        assert_eq!(with_docs.total, 2);
    }

    #[test]
    fn pick_sites_should_spread_the_cap_over_files_before_taking_a_second_site() {
        let mut rows = Vec::new();
        for line in [10, 20, 30, 40] {
            rows.push(row("src/a.rs", line, "go(LIMIT);"));
        }
        rows.push(row("src/b.rs", 7, "go(LIMIT);"));
        rows.push(row("src/c.rs", 3, "go(LIMIT);"));
        let sites = pick_sites(&rows, "LIMIT", None, None, false, 4);
        let seen: Vec<(&str, u64)> = sites
            .code
            .iter()
            .map(|s| (s.path.as_str(), s.line))
            .collect();
        assert_eq!(
            seen,
            [
                ("src/a.rs", 10),
                ("src/b.rs", 7),
                ("src/c.rs", 3),
                ("src/a.rs", 20)
            ]
        );
        assert_eq!(sites.total, 6);
    }

    #[test]
    fn pick_sites_should_keep_a_row_without_text_and_follow_the_definitions_language() {
        let rows = vec![
            RichHit {
                path: "src/a.py".into(),
                line: 4,
                text: None,
            },
            row("src/a.rs", 6, "LIMIT"),
            row("web/a.tsx", 6, "LIMIT"),
            row("web/b.ts", 6, "LIMIT"),
            row(".github/ci.yml", 6, "LIMIT"),
        ];
        let python = pick_sites(&rows, "LIMIT", None, Some("lib/x.py"), false, 8);
        assert_eq!(
            python.code,
            [Site {
                path: "src/a.py".into(),
                line: 4
            }]
        );
        let js = pick_sites(&rows, "LIMIT", None, Some("web/z.js"), false, 8);
        let paths: Vec<&str> = js.code.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["web/a.tsx", "web/b.ts"]);
        // With no definition to follow, code files of any language count and
        // configuration files do not.
        let any = pick_sites(&rows, "LIMIT", None, None, false, 8);
        assert_eq!(any.code.len(), 4);
    }

    #[test]
    fn same_language_should_treat_dialects_as_one() {
        assert!(same_language("a.ts", "b.jsx"));
        assert!(same_language("a.h", "b.cpp"));
        assert!(same_language("a.py", "b.pyi"));
        assert!(same_language("a.rs", "b.RS"));
        assert!(!same_language("a.rs", "b.py"));
        assert!(!same_language("Makefile", "b.rs"));
        assert!(is_source("src/a.rs") && !is_source("README.md") && !is_source("Cargo.toml"));
    }

    // ---- windows ---------------------------------------------------------

    #[test]
    fn site_window_should_show_two_lines_around_the_site_inside_its_function() {
        let source = api_source();
        let window = site_window(&source, 10, Detail::Lines).unwrap();
        assert_eq!((window.first, window.last), (9, 12));
        assert_eq!(window.funcs, ["search"]);
        assert!(!window.test);
        // Inside a long function the window is two lines each way.
        let mut long = lines(&["fn long() {"]);
        long.extend((1..=30).map(|n| format!("    step({n});")));
        long.push("}".to_string());
        let window = site_window(&long, 15, Detail::Lines).unwrap();
        assert_eq!((window.first, window.last), (13, 17));
    }

    #[test]
    fn site_window_should_widen_with_the_detail_and_show_a_short_function_whole() {
        let mut source = lines(&["fn long() {"]);
        source.extend((1..=60).map(|n| format!("    step({n});")));
        source.push("}".to_string());
        let near = site_window(&source, 30, Detail::Neighbourhood).unwrap();
        assert_eq!((near.first, near.last), (22, 42));
        let wide = site_window(&source, 30, Detail::Function).unwrap();
        assert_eq!((wide.first, wide.last), (15, 55));
        let api = api_source();
        let whole = site_window(&api, 10, Detail::Function).unwrap();
        assert_eq!((whole.first, whole.last), (9, 12));
    }

    #[test]
    fn site_window_should_trim_blank_edges_and_flag_test_code() {
        let source = lines(&["fn a() {}", "", "", "let x = LIMIT;", "", "", "let y = 2;"]);
        let window = site_window(&source, 4, Detail::Lines).unwrap();
        assert_eq!((window.first, window.last), (4, 4));
        let api = api_source();
        let inline = site_window(&api, 18, Detail::Lines).unwrap();
        assert!(inline.test);
        assert_eq!(inline.funcs, ["default_is_one_hundred"]);
        assert!(site_window(&api, 99, Detail::Lines).is_none());
        assert!(site_window(&api, 0, Detail::Lines).is_none());
    }

    #[test]
    fn merge_windows_should_join_overlapping_and_touching_windows_only() {
        let window = |first, last, func: &str, test| Window {
            first,
            last,
            funcs: vec![func.to_string()],
            test,
        };
        let merged = merge_windows(vec![
            window(30, 34, "c", false),
            window(10, 14, "a", false),
            window(14, 20, "b", true),
            window(21, 22, "a", false),
            window(24, 25, "d", false),
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!((merged[0].first, merged[0].last), (10, 22));
        assert_eq!(merged[0].funcs, ["a", "b"]);
        assert!(merged[0].test);
        assert_eq!((merged[1].first, merged[1].last), (24, 25));
        assert_eq!((merged[2].first, merged[2].last), (30, 34));
    }

    #[test]
    fn widen_should_stay_inside_the_file_and_the_cap() {
        assert_eq!(widen(100, 10, 20, 0), (10, 20));
        assert_eq!(widen(100, 10, 20, 5), (5, 25));
        assert_eq!(widen(100, 3, 98, 5), (1, 100));
        assert_eq!(widen(500, 100, 300, 25), (75, 75 + MAX_WIDE_LINES - 1));
        assert_eq!(widen(10, 20, 30, 0), (20, 20));
    }

    // ---- names a prompt points at ---------------------------------------

    #[test]
    fn quoted_phrases_should_take_messages_and_not_names_or_apostrophes() {
        let typed = "I keep getting 'remote decisions refused the decision: (no reason given)' when I run classify";
        assert_eq!(
            quoted_phrases(typed),
            ["remote decisions refused the decision: (no reason given)"]
        );
        assert_eq!(
            quoted_phrases("what does `merge_setting` do"),
            Vec::<String>::new()
        );
        assert_eq!(
            quoted_phrases("it's odd that I can't see what they've done"),
            Vec::<String>::new()
        );
        assert_eq!(
            quoted_phrases(
                "the \"connection reset by peer\" error and `no such file or directory`"
            ),
            ["connection reset by peer", "no such file or directory"]
        );
        assert_eq!(quoted_phrases("say 'two words' now"), Vec::<String>::new());
        assert_eq!(quoted_phrases(""), Vec::<String>::new());
    }

    #[test]
    fn phrase_needles_should_try_the_phrase_then_its_longest_segments() {
        let needles = phrase_needles("remote decisions refused the decision: (no reason given)");
        assert_eq!(
            needles,
            [
                "remote decisions refused the decision: (no reason given)",
                "remote decisions refused the decision",
                "no reason given",
            ]
        );
        assert_eq!(phrase_needles("one two three four"), ["one two three four"]);
    }

    #[test]
    fn rank_identifiers_should_find_the_name_that_spells_the_prompts_words() {
        let hints = lines(&[
            "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            "fn search_rows(data: &Value) -> RichFound {",
            "let unrelated = Other::new();",
        ]);
        let terms = lines(&["default", "number", "rows", "search", "returns"]);
        assert_eq!(
            rank_identifiers(&hints, &terms),
            ["SEARCH_DEFAULT_ROWS", "search_rows"]
        );
        // A name that spells one word is not enough; no word, nothing.
        assert!(rank_identifiers(&hints, &lines(&["rows"])).is_empty());
        assert!(rank_identifiers(&[], &terms).is_empty());
        // Ties at the best overlap all stay; weaker names only fill two places.
        let hints = lines(&["fn a_b_c() {} fn a_b_x() {} fn a_b_y() {} fn a_z() {}"]);
        let names = rank_identifiers(&hints, &lines(&["a", "b", "c", "x", "y", "z"]));
        assert_eq!(names.len(), 3);
        assert_eq!(names[0], "a_b_c");
    }

    #[test]
    fn parse_test_entry_should_split_path_line_and_name() {
        assert_eq!(
            parse_test_entry("tests/t.rs:12 default_is_one — assert_eq!(a, b)"),
            Some(("tests/t.rs", 12, "default_is_one"))
        );
        assert_eq!(
            parse_test_entry("tests/t.rs:12 name"),
            Some(("tests/t.rs", 12, "name"))
        );
        assert_eq!(parse_test_entry("no place here"), None);
        assert_eq!(parse_test_entry("t.rs:x name"), None);
    }

    #[test]
    fn block_range_and_overlaps_should_read_the_head_line() {
        let block = Block::new("a/b.rs:10-20 — f definition (function)".into(), Vec::new());
        assert_eq!(block_range(&block), Some(("a/b.rs".to_string(), 10, 20)));
        let taken = vec![("a/b.rs".to_string(), 1, 10)];
        assert!(overlaps(&taken, &block));
        let taken = vec![("a/b.rs".to_string(), 21, 30), ("c.rs".to_string(), 1, 99)];
        assert!(!overlaps(&taken, &block));
        let tests = Block::new("tests of x".into(), Vec::new());
        assert_eq!(block_range(&tests), None);
        assert!(!overlaps(&taken, &tests));
    }

    #[test]
    fn unseen_tests_should_keep_each_entry_and_body_once() {
        let list = |entries: &[&str]| Candidate {
            priority: 0,
            order: (0, 2, 0),
            part: Part::Tests,
            block: Block::new("tests of x".into(), lines(entries)),
        };
        let body = |head: &str| Candidate {
            priority: 0,
            order: (0, 3, 0),
            part: Part::TestBody,
            block: Block::new(head.into(), lines(&["1| a"])),
        };
        let kept = unseen_tests(vec![
            list(&["a.rs:1 t1", "a.rs:2 t2"]),
            list(&["a.rs:2 t2"]),
            list(&["a.rs:2 t2", "a.rs:3 t3"]),
            body("a.rs:1-2 — t1"),
            body("a.rs:1-2 — t1"),
        ]);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].block.body, ["a.rs:1 t1", "a.rs:2 t2"]);
        assert_eq!(kept[1].block.body, ["a.rs:3 t3"]);
        assert_eq!(kept[2].part, Part::TestBody);
    }

    // ---- the builder -----------------------------------------------------

    /// A source of files, rows and symbols.
    #[derive(Default)]
    struct Repo {
        sources: Vec<(String, Vec<String>)>,
        rows: Vec<(String, Vec<RichHit>)>,
        symbols: Vec<(String, Result<Vec<SymbolHit>, String>)>,
    }

    impl Evidence for Repo {
        fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
            self.files_matching(anchor, deadline).map(Found::from)
        }
        fn files_matching(
            &self,
            anchor: &str,
            _: Instant,
        ) -> Result<crate::execution_brief::chain::RichFound, String> {
            Ok(crate::execution_brief::chain::RichFound {
                hits: self
                    .rows
                    .iter()
                    .find(|(name, _)| name == anchor)
                    .map(|(_, rows)| rows.clone())
                    .unwrap_or_default(),
                capped: false,
            })
        }
        fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("unsupported".into())
        }
        fn symbols(&self, name: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            self.symbols
                .iter()
                .find(|(held, _)| held == name)
                .map_or_else(|| Ok(Vec::new()), |(_, answer)| answer.clone())
        }
        fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            Ok(Vec::new())
        }
        fn lines_at(
            &self,
            path: &str,
            start: u64,
            end: u64,
            _: Instant,
        ) -> Result<Vec<String>, String> {
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

    fn symbol_hit(path: &str, name: &str, kind: &str, start: u64, end: u64) -> SymbolHit {
        SymbolHit {
            uid: format!("{path}#{name}#{kind}"),
            name: name.to_string(),
            kind: kind.to_string(),
            path: path.to_string(),
            start_line: start,
            end_line: end,
        }
    }

    /// The constant of `api_source`, used by a function, a second file and a
    /// test file.
    fn repo() -> Repo {
        let main = lines(&[
            "use api::search;",
            "",
            "fn handler(req: Request) -> usize {",
            "    let rows = SEARCH_DEFAULT_ROWS;",
            "    search(Some(rows))",
            "}",
        ]);
        let tests = lines(&[
            "use api::SEARCH_DEFAULT_ROWS;",
            "",
            "#[test]",
            "fn search_returns_the_default() {",
            "    assert_eq!(SEARCH_DEFAULT_ROWS, 100);",
            "}",
        ]);
        let usage = vec![
            row(
                "src/api.rs",
                5,
                "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            ),
            row(
                "src/api.rs",
                10,
                "    let wanted = limit.unwrap_or(SEARCH_DEFAULT_ROWS);",
            ),
            row("src/main.rs", 4, "    let rows = SEARCH_DEFAULT_ROWS;"),
            row(
                "tests/api_test.rs",
                5,
                "    assert_eq!(SEARCH_DEFAULT_ROWS, 100);",
            ),
            row("docs/api.md", 3, "SEARCH_DEFAULT_ROWS is 100"),
        ];
        Repo {
            sources: vec![
                ("src/api.rs".into(), api_source()),
                ("src/main.rs".into(), main),
                ("tests/api_test.rs".into(), tests),
            ],
            rows: vec![("SEARCH_DEFAULT_ROWS".into(), usage)],
            symbols: vec![(
                "SEARCH_DEFAULT_ROWS".into(),
                Ok(vec![symbol_hit(
                    "src/api.rs",
                    "SEARCH_DEFAULT_ROWS",
                    "const",
                    5,
                    5,
                )]),
            )],
        }
    }

    fn input(parts: usize) -> Input {
        Input {
            names: vec!["SEARCH_DEFAULT_ROWS".into()],
            phrases: Vec::new(),
            paths: Vec::new(),
            defined: vec![site("src/api.rs", 5, Some(5), "const")],
            graph_down: false,
            first_rows: None,
            callers: Vec::new(),
            reads: Vec::new(),
            regions: false,
            hints: Vec::new(),
            terms: Vec::new(),
            facts: Vec::new(),
            wants_tests: false,
            wants_docs: false,
            locators: Vec::new(),
            limit: Limit::of(parts),
            seen: Seen::default(),
        }
    }

    fn built(input: &Input, repo: &Repo) -> Option<Pack> {
        build(
            input,
            repo,
            Instant::now() + Duration::from_secs(10),
            &mut |_| {},
        )
    }

    fn heads(pack: &Pack) -> Vec<String> {
        pack.text()
            .lines()
            .filter(|line| line.starts_with("### "))
            .map(|line| line[4..].to_string())
            .collect()
    }

    #[test]
    fn build_should_show_the_definition_its_usage_in_functions_and_its_tests() {
        let pack = built(&input(1), &repo()).unwrap();
        let heads = heads(&pack);
        // A constant sits among its siblings: with room to spare, they come too.
        assert_eq!(
            heads[0],
            "src/api.rs:1-20 — SEARCH_DEFAULT_ROWS definition (const) with its neighbours"
        );
        // The use in `search` sits inside the lines the definition brings.
        assert!(
            !heads.iter().any(|head| head.contains("usage in search")),
            "{heads:?}"
        );
        assert_eq!(
            pack.text()
                .matches("limit.unwrap_or(SEARCH_DEFAULT_ROWS)")
                .count(),
            1
        );
        assert!(
            heads
                .iter()
                .any(|head| head.contains("src/main.rs:") && head.contains("usage in handler")),
            "{heads:?}"
        );
        assert!(
            heads.contains(&"tests of SEARCH_DEFAULT_ROWS".to_string()),
            "{heads:?}"
        );
        let text = pack.text();
        assert!(text.starts_with("[PIXEL:BRIEF]\nregion pack — searched SEARCH_DEFAULT_ROWS in the symbol graph and text index: 1 definition(s)"));
        assert!(text.contains(
            "tests/api_test.rs:4 search_returns_the_default — assert_eq!(SEARCH_DEFAULT_ROWS, 100);"
        ));
        assert!(text.contains("5| pub const SEARCH_DEFAULT_ROWS: usize = 100;"));
        assert!(text.ends_with(DIRECTIVE));
        assert!(!text.contains("docs/api.md"));
        assert!(pack.has_evidence());
        assert_eq!(pack.targets(), 1);
        assert!(!pack.fallback());
    }

    #[test]
    fn a_stale_pack_should_keep_its_notice_and_drop_whole_blocks_to_fit() {
        let pack = built(&input(1), &repo()).unwrap();
        let notice = "staleness: evidence predates the last edit";
        let roomy = pack.text_with_notice(notice, Limit::of(1)).unwrap();
        assert!(roomy.starts_with(&format!("{BRIEF_TAG}\n{notice}\n")));
        assert!(roomy.ends_with(LEAD));
        assert_eq!(roomy.matches("### ").count(), pack.blocks.len());
        let limit = Limit {
            parts: 1,
            chars: roomy.len() - 1,
        };
        let tight = pack.text_with_notice(notice, limit).unwrap();
        assert!(fits(&tight, limit), "{tight}");
        assert!(tight.contains(notice));
        assert!(tight.ends_with(INCOMPLETE));
        assert_eq!(
            pack.text_with_notice(notice, Limit { parts: 1, chars: 1 }),
            None
        );
        assert!(tight.matches("### ").count() < pack.blocks.len());
    }

    #[test]
    fn a_pack_of_a_cut_packet_should_end_on_the_lead_not_the_directive() {
        let pack = built(&input(1), &repo()).unwrap();
        assert!(pack.text_hedged(false).ends_with(DIRECTIVE));
        assert!(pack.text_hedged(true).ends_with(LEAD));
        assert_eq!(pack.text(), pack.text_hedged(false));
        // Only the last line differs.
        let plain = pack.text_hedged(false);
        let hedged = pack.text_hedged(true);
        assert_eq!(
            plain.rsplit_once('\n').unwrap().0,
            hedged.rsplit_once('\n').unwrap().0
        );
        let early = Pack {
            complete: false,
            ..pack
        };
        assert!(early.text_hedged(true).ends_with(INCOMPLETE));
    }

    #[test]
    fn build_should_not_show_a_usage_that_is_the_definition_itself() {
        let pack = built(&input(1), &repo()).unwrap();
        let usage: Vec<String> = heads(&pack)
            .into_iter()
            .filter(|h| h.contains(" usage"))
            .collect();
        assert!(
            usage.iter().all(|head| !head.starts_with("src/api.rs:4-5")),
            "{usage:?}"
        );
    }

    #[test]
    fn build_should_show_more_of_each_function_when_the_parts_have_room() {
        let pack = built(&input(3), &repo()).unwrap();
        // The whole of `handler`, not two lines each way.
        assert!(
            pack.text()
                .contains("3| fn handler(req: Request) -> usize {")
        );
        assert!(pack.text().contains("6| }"));
    }

    #[test]
    fn build_should_publish_the_pack_as_it_grows_and_mark_only_the_last_complete() {
        let repo = repo();
        let mut seen: Vec<(bool, usize)> = Vec::new();
        let last = build(
            &input(1),
            &repo,
            Instant::now() + Duration::from_secs(10),
            &mut |pack| seen.push((pack.complete, pack.blocks.len())),
        )
        .unwrap();
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|(complete, _)| !complete));
        assert!(last.complete);
        assert!(!last.blocks.is_empty());
        let early = Pack {
            complete: false,
            ..last.clone()
        };
        assert!(early.text().ends_with(INCOMPLETE));
        assert!(!early.text().contains(DIRECTIVE));
    }

    #[test]
    fn build_should_return_nothing_when_the_window_is_over() {
        let repo = repo();
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(build(&input(1), &repo, past, &mut |_| {}).is_none());
    }

    #[test]
    fn build_should_fit_the_parts_it_is_given() {
        let mut big = repo();
        // A symbol with a sixty-line body, used from many files.
        let mut body = lines(&["pub fn big() {"]);
        body.extend((1..=70).map(|n| format!("    // line {n} {}", "z".repeat(100))));
        body.push("}".to_string());
        big.sources.push(("src/big.rs".into(), body));
        let mut rows = vec![row("src/big.rs", 1, "pub fn big() {")];
        for file in 0..12 {
            let mut caller = lines(&["fn caller() {"]);
            caller.extend((1..=30).map(|n| format!("    other({n}); // {}", "c".repeat(100))));
            caller[10] = "    big();".to_string();
            caller.push("}".to_string());
            let path = format!("src/c{file}.rs");
            big.sources.push((path.clone(), caller));
            rows.push(row(&path, 11, "    big();"));
        }
        big.rows.push(("big".into(), rows));
        big.symbols.push((
            "big".into(),
            Ok(vec![symbol_hit("src/big.rs", "big", "function", 1, 72)]),
        ));
        let mut wanted = input(1);
        wanted.names = vec!["big".into()];
        wanted.defined = vec![site("src/big.rs", 1, Some(72), "function")];
        for parts in 1..=MAX_PARTS {
            wanted.limit = Limit::of(parts);
            let pack = built(&wanted, &big).unwrap();
            let cut = split_parts(&pack.text(), PART_CHARS);
            assert!(cut.len() <= parts, "{parts}: {}", cut.len());
            assert!(cut.iter().all(|part| part.len() <= PART_CHARS));
            assert!(
                pack.text().contains("1| pub fn big() {"),
                "{parts}: the definition always stays"
            );
        }
        wanted.limit = Limit::of(1);
        let one = built(&wanted, &big).unwrap().text().len();
        wanted.limit = Limit::of(3);
        let three = built(&wanted, &big).unwrap().text().len();
        assert!(one <= PART_CHARS && three > one, "{one} {three}");
    }

    #[test]
    fn build_should_fall_back_to_the_text_search_when_the_graph_cannot_answer() {
        let mut stale = input(1);
        stale.defined = Vec::new();
        stale.graph_down = true;
        let pack = built(&stale, &repo()).unwrap();
        assert!(pack.fallback());
        assert!(
            pack.text().contains("graph stale: text fallback"),
            "{}",
            pack.text()
        );
        assert_eq!(
            heads(&pack)[0],
            "src/api.rs:1-20 — SEARCH_DEFAULT_ROWS definition (value) with its neighbours"
        );
        // A graph that answered "nothing" is not a stale graph.
        let mut empty = input(1);
        empty.defined = Vec::new();
        let pack = built(&empty, &repo()).unwrap();
        assert!(!pack.fallback());
        assert!(!pack.text().contains("graph stale"));
        assert!(heads(&pack)[0].starts_with("src/api.rs:1-20 — SEARCH_DEFAULT_ROWS definition"));
    }

    #[test]
    fn build_should_show_the_matches_of_a_name_with_no_declaration() {
        let mut none = input(1);
        none.defined = Vec::new();
        let mut repo = repo();
        repo.rows[0].1.remove(0);
        repo.rows[0]
            .1
            .retain(|hit| hit.path != "src/api.rs" || hit.line != 5);
        let pack = built(&none, &repo).unwrap();
        let heads = heads(&pack);
        assert!(heads[0].contains("SEARCH_DEFAULT_ROWS match"), "{heads:?}");
        assert!(pack.has_evidence());
    }

    #[test]
    fn build_should_search_a_quoted_message_when_the_prompt_names_no_symbol() {
        let source = lines(&[
            "fn decide() -> Result<(), String> {",
            "    let reason = reply.reason();",
            "    return Err(format!(\"remote decisions refused the decision: {reason}\"));",
            "}",
        ]);
        let repo = Repo {
            sources: vec![("src/decide.rs".into(), source)],
            rows: vec![
                (
                    "remote decisions refused the decision: (no reason given)".into(),
                    vec![row(
                        ".agents/rules/project-task.md",
                        75,
                        "`remote decisions refused the decision: (no reason given)`",
                    )],
                ),
                (
                    "remote decisions refused the decision".into(),
                    vec![row(
                        "src/decide.rs",
                        3,
                        "    return Err(format!(\"remote decisions refused the decision: {reason}\"));",
                    )],
                ),
            ],
            symbols: Vec::new(),
        };
        let mut message = input(1);
        message.names = Vec::new();
        message.defined = Vec::new();
        message.phrases = vec!["remote decisions refused the decision: (no reason given)".into()];
        let pack = built(&message, &repo).unwrap();
        assert_eq!(
            heads(&pack)[0],
            "src/decide.rs:1-4 — remote decisions refused the decision match"
        );
        assert!(pack.text().ends_with(DIRECTIVE));
    }

    #[test]
    fn build_should_leave_regions_and_spelled_names_out_when_something_better_matched() {
        let mut named = input(3);
        named.regions = true;
        named.hints = lines(&["pub const SEARCH_MAX_ROWS: usize = 10_000;"]);
        named.terms = lines(&["search", "rows", "max"]);
        named.reads = vec![ReadRegion {
            path: "src/main.rs".into(),
            first: 1,
            last: 6,
            symbol: None,
        }];
        let pack = built(&named, &repo()).unwrap();
        assert_eq!(pack.targets(), 1);
        assert!(
            !pack.text().contains("SEARCH_MAX_ROWS definition"),
            "{}",
            pack.text()
        );
        assert!(!heads(&pack).iter().any(|head| head.contains(" region")));
        assert!(pack.text().ends_with(DIRECTIVE));
    }

    #[test]
    fn build_should_spell_a_name_from_the_best_files_when_the_prompt_gave_none() {
        let mut prose = input(3);
        prose.names = Vec::new();
        prose.defined = Vec::new();
        prose.regions = true;
        prose.hints = lines(&["pub const SEARCH_DEFAULT_ROWS: usize = 100;"]);
        prose.terms = lines(&["default", "rows", "search"]);
        let pack = built(&prose, &repo()).unwrap();
        assert_eq!(pack.targets(), 1);
        assert!(heads(&pack)[0].contains("SEARCH_DEFAULT_ROWS definition (const)"));
        assert!(
            pack.text().ends_with(DIRECTIVE),
            "a spelled name is still a name"
        );
    }

    #[test]
    fn build_should_end_a_pack_of_regions_alone_on_a_lead_not_a_directive() {
        let mut regions = input(1);
        regions.names = Vec::new();
        regions.defined = Vec::new();
        regions.regions = true;
        regions.reads = vec![ReadRegion {
            path: "src/api.rs".into(),
            first: 9,
            last: 12,
            symbol: Some("search".into()),
        }];
        let pack = built(&regions, &repo()).unwrap();
        assert!(pack.text().ends_with(LEAD), "{}", pack.text());
        assert!(!pack.text().contains(DIRECTIVE));
        // A short file is shown whole, the way the best file is.
        assert!(pack.text().contains("1| //! The wire contract."));
        assert_eq!(split_parts(&pack.text(), PART_CHARS).len(), 1);
    }

    #[test]
    fn build_should_follow_the_callers_of_a_function_and_the_tests_that_reach_it() {
        let lib = lines(&[
            "pub fn merge_setting(a: u32, b: u32) -> u32 {",
            "    a + b",
            "}",
            "",
            "pub fn prepare(a: u32) -> u32 {",
            "    merge_setting(a, 1)",
            "}",
        ]);
        let session = lines(&[
            "fn request() -> u32 {",
            "    let p = prepare(2);",
            "    p",
            "}",
        ]);
        let tests = lines(&[
            "#[test]",
            "fn prepare_merges_settings() {",
            "    assert_eq!(prepare(1), 2);",
            "}",
        ]);
        let repo = Repo {
            sources: vec![
                ("src/lib.rs".into(), lib),
                ("src/session.rs".into(), session),
                ("tests/prep_test.rs".into(), tests),
            ],
            rows: vec![
                (
                    "merge_setting".into(),
                    vec![
                        row(
                            "src/lib.rs",
                            1,
                            "pub fn merge_setting(a: u32, b: u32) -> u32 {",
                        ),
                        row("src/lib.rs", 6, "    merge_setting(a, 1)"),
                    ],
                ),
                (
                    "prepare".into(),
                    vec![
                        row("src/lib.rs", 5, "pub fn prepare(a: u32) -> u32 {"),
                        row("src/session.rs", 2, "    let p = prepare(2);"),
                        row("tests/prep_test.rs", 3, "    assert_eq!(prepare(1), 2);"),
                    ],
                ),
            ],
            symbols: vec![(
                "merge_setting".into(),
                Ok(vec![symbol_hit(
                    "src/lib.rs",
                    "merge_setting",
                    "function",
                    1,
                    3,
                )]),
            )],
        };
        let mut change = input(3);
        change.names = vec!["merge_setting".into()];
        change.defined = vec![site("src/lib.rs", 1, Some(3), "function")];
        let pack = built(&change, &repo).unwrap();
        let text = pack.text();
        assert!(
            text.contains("prepare usage in request (reaches merge_setting)"),
            "{text}"
        );
        assert!(
            text.contains("2|     let p = prepare(2);") || text.contains("2| let p = prepare(2);"),
            "{text}"
        );
        assert!(
            text.contains("tests reaching merge_setting through its callers"),
            "{text}"
        );
        assert!(
            text.contains(
                "tests/prep_test.rs:2 prepare_merges_settings — assert_eq!(prepare(1), 2);"
            ),
            "{text}"
        );
        assert!(text.contains("(a test of merge_setting)"), "{text}");
    }

    #[test]
    fn build_should_refuse_a_generic_name_with_too_many_declarations() {
        let mut generic = repo();
        generic.symbols = vec![(
            "SEARCH_DEFAULT_ROWS".into(),
            Ok((0..=AMBIGUOUS_DEFINITIONS)
                .map(|n| {
                    symbol_hit(
                        &format!("src/m{n}.rs"),
                        "SEARCH_DEFAULT_ROWS",
                        "function",
                        1,
                        2,
                    )
                })
                .collect()),
        )];
        let mut many = input(1);
        many.defined = (0..=AMBIGUOUS_DEFINITIONS)
            .map(|n| site(&format!("src/m{n}.rs"), 1, Some(2), "function"))
            .collect();
        assert!(built(&many, &generic).is_none());
        // A path anchor settles it.
        many.paths = vec!["src/m3.rs".into()];
        generic
            .sources
            .push(("src/m3.rs".into(), lines(&["fn a() {}", "fn b() {}"])));
        let pack = built(&many, &generic).unwrap();
        assert!(heads(&pack)[0].starts_with("src/m3.rs:1-2"));
    }

    #[test]
    fn build_should_carry_graph_facts_and_the_callers_the_graph_reported() {
        let mut facts = input(1);
        facts.facts = vec!["flow: a -> b -> c".into()];
        facts.callers = vec![("src/main.rs".into(), 4)];
        let pack = built(&facts, &repo()).unwrap();
        assert!(
            pack.text()
                .contains("### call graph facts\nflow: a -> b -> c")
        );
        assert!(
            heads(&pack)
                .iter()
                .any(|head| head.starts_with("src/main.rs:") && head.contains("usage in handler"))
        );
    }

    // ---- locators ----------------------------------------------------------

    fn located(head: &str, line: &str) -> Block {
        Block::new(head.into(), lines(&[line]))
    }

    #[test]
    fn locator_blocks_should_stand_beside_the_symbols_and_count_as_evidence() {
        let mut with = input(1);
        with.locators = vec![located("src/conf.py:3-3 — MAX (constant)", "3| MAX = 3")];
        let pack = built(&with, &repo()).unwrap();
        let heads = heads(&pack);
        // After the symbol's own blocks, before nothing but the graph facts.
        assert_eq!(
            heads.last().map(String::as_str),
            Some("src/conf.py:3-3 — MAX (constant)")
        );
        assert_eq!(pack.located(), 1);
        assert!(
            pack.text().contains(", 1 located block(s)."),
            "{}",
            pack.text()
        );
        assert!(pack.text().ends_with(DIRECTIVE));
        // Without locators the header says nothing of them.
        let plain = built(&input(1), &repo()).unwrap();
        assert_eq!(plain.located(), 0);
        assert!(!plain.text().contains("located block"));
    }

    #[test]
    fn a_pack_of_locator_blocks_alone_is_evidence_and_an_anchored_answer() {
        // A prompt that names no symbol: nothing resolves, the locators stand
        // for the whole pack, and they are found by the prompt's own words.
        let mut alone = input(1);
        alone.names = Vec::new();
        alone.defined = Vec::new();
        alone.locators = vec![located(
            "tests/t.py:9-9 — test_x (test)",
            "9| def test_x():",
        )];
        let pack = built(&alone, &repo()).unwrap();
        assert!(pack.has_evidence());
        assert_eq!(pack.targets(), 0);
        assert!(
            pack.text()
                .contains("searched the words of the question in")
        );
        assert!(pack.text().ends_with(DIRECTIVE), "{}", pack.text());
        // No symbol and no locator: nothing is worth showing.
        alone.locators = Vec::new();
        assert!(built(&alone, &repo()).is_none());
    }

    #[test]
    fn a_locator_block_over_lines_a_definition_shows_is_not_shown_twice() {
        let mut with = input(1);
        with.locators = vec![
            located(
                "src/api.rs:5-5 — SEARCH_DEFAULT_ROWS (constant)",
                "5| pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            ),
            located("src/other.rs:1-1 — OTHER (constant)", "1| OTHER = 1"),
        ];
        let pack = built(&with, &repo()).unwrap();
        let heads = heads(&pack);
        assert!(
            !heads
                .iter()
                .any(|head| head.contains("(constant)") && head.starts_with("src/api.rs:5-5")),
            "{heads:?}"
        );
        assert!(
            heads
                .iter()
                .any(|head| head.starts_with("src/other.rs:1-1")),
            "{heads:?}"
        );
        assert_eq!(pack.located(), 1);
    }

    #[test]
    fn a_pack_should_leave_out_the_ranges_a_session_was_sent_and_keep_the_rest() {
        let repo = repo();
        let first = built(&input(1), &repo).expect("a pack");
        let ranges = first.ranges();
        let in_file = |path: &str| -> Vec<(String, u64, u64)> {
            ranges
                .iter()
                .filter(|(held, _, _)| held == path)
                .cloned()
                .collect()
        };
        assert!(
            !in_file("src/api.rs").is_empty() && !in_file("src/main.rs").is_empty(),
            "{ranges:?}"
        );
        // Everything sent: the same pack says nothing new.
        let mut all = input(1);
        all.seen = Seen::default().with(&ranges, 0);
        assert!(built(&all, &repo).is_none_or(|pack| !pack.has_evidence()));
        // Only the usage in main.rs sent: the definition is still shown, the usage is not.
        let mut some = input(1);
        some.seen = Seen::default().with(&in_file("src/main.rs"), 0);
        let rest = built(&some, &repo)
            .expect("the definition remains")
            .ranges();
        assert!(
            rest.iter().all(|(path, _, _)| path != "src/main.rs"),
            "{rest:?}"
        );
        assert!(
            rest.iter().any(|(path, _, _)| path == "src/api.rs"),
            "{rest:?}"
        );
        // Another file of the same name is another file.
        let mut other = input(1);
        other.seen = Seen::default().with(&[("elsewhere/main.rs".to_string(), 1, 1_000)], 0);
        assert_eq!(built(&other, &repo).unwrap().ranges(), ranges);
    }

    #[test]
    fn the_room_a_sent_site_would_take_should_go_to_a_site_the_session_has_not_got() {
        // Ten files use the constant; a pack shows eight sites at most.
        let usage: Vec<RichHit> = (0..10)
            .map(|n| {
                row(
                    &format!("src/user_{n}.rs"),
                    2,
                    "    let x = SEARCH_DEFAULT_ROWS;",
                )
            })
            .collect();
        let sources: Vec<(String, Vec<String>)> = (0..10)
            .map(|n| {
                (
                    format!("src/user_{n}.rs"),
                    lines(&["fn run() {", "    let x = SEARCH_DEFAULT_ROWS;", "}"]),
                )
            })
            .chain(std::iter::once(("src/api.rs".to_string(), api_source())))
            .collect();
        let repo = Repo {
            sources,
            rows: vec![("SEARCH_DEFAULT_ROWS".into(), usage)],
            symbols: repo().symbols,
        };
        let first = built(&input(3), &repo).expect("a pack").ranges();
        let used = |ranges: &[(String, u64, u64)]| {
            ranges
                .iter()
                .filter(|(path, _, _)| path.starts_with("src/user_"))
                .count()
        };
        let sent = used(&first);
        assert!((1..=MAX_SITES).contains(&sent), "{first:?}");
        let mut again = input(3);
        again.seen = Seen::default().with(&first, 0);
        let second = built(&again, &repo).expect("new sites remain").ranges();
        assert!(used(&second) > 0, "{second:?}");
        assert!(
            second.iter().all(|(path, a, b)| !first
                .iter()
                .any(|(held, x, y)| held == path && x <= b && a <= y)),
            "nothing is sent twice: {second:?} against {first:?}"
        );
    }

    #[test]
    fn audited_whole_file_chunks_should_preserve_exact_priorities_ranges_and_source_lines() {
        let source = (1..=121).map(|n| format!("line-{n}")).collect::<Vec<_>>();
        let blocks = whole_file_blocks(2, "source.rs", &source);
        assert_eq!(
            blocks
                .iter()
                .map(|c| (c.priority, c.order, block_range(&c.block).unwrap()))
                .collect::<Vec<_>>(),
            [
                (3, (2, 0, 1), ("source.rs".into(), 1, 60)),
                (10, (2, 0, 61), ("source.rs".into(), 61, 120)),
                (13, (2, 0, 121), ("source.rs".into(), 121, 121)),
            ]
        );
        assert_eq!(
            blocks
                .iter()
                .map(|c| c.block.body.len())
                .collect::<Vec<_>>(),
            [60, 60, 1]
        );
        assert_eq!(blocks[2].block.body, ["121| line-121"]);
    }

    #[test]
    fn audited_selection_should_add_the_chosen_ladders_site_counts_to_base_and_tests() {
        let candidate = |head: &str| Candidate {
            priority: 0,
            order: (0, 0, 0),
            part: Part::Definition,
            block: Block::new(head.into(), vec![]),
        };
        let base = [candidate("base")];
        let tests = [candidate("tests")];
        let levels = vec![
            vec![
                Level {
                    blocks: vec![candidate("a0")],
                    sites: 2,
                },
                Level {
                    blocks: vec![candidate("a1")],
                    sites: 5,
                },
            ],
            vec![Level {
                blocks: vec![candidate("b0")],
                sites: 3,
            }],
        ];
        let (chosen, sites) = select(&base, &tests, &levels, &[1, 0]);
        assert_eq!(sites, 8);
        assert_eq!(
            chosen
                .iter()
                .map(|c| c.block.head.as_str())
                .collect::<Vec<_>>(),
            ["base", "tests", "a1", "b0"]
        );
    }

    #[test]
    fn audited_test_bodies_should_deduplicate_by_head_even_when_the_contents_differ() {
        let body = |head: &str, line: &str| Candidate {
            priority: 40,
            order: (0, 3, 1),
            part: Part::TestBody,
            block: Block::new(head.into(), vec![line.into()]),
        };
        let found = unseen_tests(vec![
            body("a.rs:1-2 — first", "1| first"),
            body("a.rs:1-2 — first", "1| changed"),
            body("a.rs:4-5 — second", "1| first"),
        ]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].block.body, ["1| first"]);
        assert_eq!(found[1].block.head, "a.rs:4-5 — second");
    }

    #[test]
    fn audited_test_candidates_should_cap_unique_paths_and_rank_lists_before_exact_test_bodies() {
        let mut evidence = Repo::default();
        for n in 0..4 {
            evidence.sources.push((
                format!("tests/direct{n}.rs"),
                lines(&[
                    "#[test]",
                    "fn direct() {",
                    "    assert_eq!(SEARCH_DEFAULT_ROWS, 100);",
                    "}",
                ]),
            ));
        }
        for n in 0..3 {
            evidence.sources.push((
                format!("tests/hop{n}.rs"),
                lines(&[
                    "#[test]",
                    "fn through() {",
                    "    assert_eq!(handler(), 100);",
                    "}",
                ]),
            ));
        }
        evidence.rows.push((
            "handler".into(),
            (0..3)
                .map(|n| {
                    row(
                        &format!("tests/hop{n}.rs"),
                        3,
                        "    assert_eq!(handler(), 100);",
                    )
                })
                .collect(),
        ));
        let input = input(4);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut files = Files::new(&evidence, deadline);
        let target = Target {
            name: "SEARCH_DEFAULT_ROWS".into(),
            definition: None,
            region: None,
            rows: vec![],
            by_text: false,
            sites: 8,
            matches: false,
            whole_file: false,
        };
        let sites = Sites {
            tests: vec![
                Site {
                    path: "tests/direct0.rs".into(),
                    line: 3,
                },
                Site {
                    path: "tests/direct0.rs".into(),
                    line: 3,
                },
                Site {
                    path: "tests/direct1.rs".into(),
                    line: 3,
                },
                Site {
                    path: "tests/direct2.rs".into(),
                    line: 3,
                },
                Site {
                    path: "tests/direct3.rs".into(),
                    line: 3,
                },
            ],
            ..Sites::default()
        };
        let mut context = Context {
            input: &input,
            deadline,
            files: &mut files,
        };
        let found = test_candidates(2, &target, &sites, &[], &["handler".into()], &mut context);
        assert_eq!(
            found
                .iter()
                .map(|c| (c.priority, c.order, c.part))
                .collect::<Vec<_>>(),
            [
                (5, (2, 2, 0), Part::Tests),
                (8, (2, 2, 1), Part::Tests),
                (42, (2, 3, 2), Part::TestBody),
                (45, (2, 3, 2), Part::TestBody),
                (48, (2, 3, 2), Part::TestBody)
            ]
        );
        assert_eq!(found[0].block.body.len(), 3);
        assert_eq!(found[1].block.body.len(), 2);
        assert!(
            found[0]
                .block
                .body
                .iter()
                .all(|entry| !entry.contains("direct3"))
        );
        assert!(
            found[1]
                .block
                .body
                .iter()
                .all(|entry| !entry.contains("hop2"))
        );
        assert_eq!(
            found[2].block.body,
            [
                "2| fn direct() {",
                "3|     assert_eq!(SEARCH_DEFAULT_ROWS, 100);",
                "4| }"
            ]
        );
        let refs = found.iter().collect::<Vec<_>>();
        let text = header(&input, &[target], &refs, 0, false);
        assert!(text.contains("5 test(s) listed"), "{text}");
    }

    #[test]
    fn audited_definition_and_usage_candidates_should_preserve_target_rank_and_display_order() {
        let evidence = repo();
        let input = input(4);
        let deadline = Instant::now() + Duration::from_secs(5);
        let targets = resolve_targets(&input, &evidence, deadline);
        let target = &targets[0];
        let mut files = Files::new(&evidence, deadline);
        let definitions = definition_candidates(2, target, 0, &mut files);
        assert_eq!(
            definitions
                .iter()
                .map(|c| (c.priority, c.order, c.part))
                .collect::<Vec<_>>(),
            [(2, (2, 0, 5), Part::Definition)]
        );
        assert!(definitions[0].block.head.ends_with("definition (const)"));
        let mut context = Context {
            input: &input,
            deadline,
            files: &mut files,
        };
        let (usages, total, _) = usage_candidates(2, target, Detail::Lines, &mut context);
        assert_eq!(total, 2);
        assert_eq!(
            usages.iter().map(|c| c.priority).collect::<Vec<_>>(),
            [9, 12]
        );
        assert!(
            usages
                .iter()
                .all(|c| c.part == Part::Usage && c.order.0 == 2 && c.order.1 == 1)
        );
    }

    #[test]
    fn audited_caller_candidates_should_rank_both_hops_and_skip_the_functions_own_declaration() {
        let mut evidence = Repo::default();
        for (path, name) in [("first.rs", "dispatch"), ("second.rs", "serve")] {
            evidence.sources.push((
                path.into(),
                lines(&[&format!("fn {name}() {{"), "    handler();", "}"]),
            ));
        }
        evidence.sources.push((
            "own.rs".into(),
            lines(&["fn handler() {", "    work();", "}"]),
        ));
        evidence.rows.push((
            "handler".into(),
            vec![
                row("first.rs", 2, "    handler();"),
                row("second.rs", 2, "    handler();"),
                row("own.rs", 1, "fn handler() {"),
            ],
        ));
        let input = input(4);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut files = Files::new(&evidence, deadline);
        let target = Target {
            name: "target".into(),
            definition: None,
            region: None,
            rows: vec![],
            by_text: false,
            sites: 8,
            matches: false,
            whole_file: false,
        };
        let mut context = Context {
            input: &input,
            deadline,
            files: &mut files,
        };
        let found = caller_candidates(
            2,
            &target,
            &["handler".into()],
            None,
            Detail::Lines,
            &mut context,
        );
        assert_eq!(
            found.iter().map(|c| c.priority).collect::<Vec<_>>(),
            [33, 36]
        );
        assert_eq!(
            found
                .iter()
                .map(|c| block_range(&c.block).unwrap().0)
                .collect::<Vec<_>>(),
            ["first.rs", "second.rs"]
        );
    }

    #[test]
    fn audited_match_candidates_should_take_two_unique_code_files_in_target_rank_order() {
        let mut evidence = Repo::default();
        let mut rows = Vec::new();
        for name in ["first", "second", "third"] {
            let path = format!("{name}.rs");
            evidence.sources.push((
                path.clone(),
                lines(&["fn caller() {", "    needle();", "}"]),
            ));
            rows.push(row(&path, 2, "    needle();"));
        }
        rows.insert(1, rows[0].clone());
        let target = Target {
            name: "needle".into(),
            definition: None,
            region: None,
            rows,
            by_text: true,
            sites: 8,
            matches: true,
            whole_file: false,
        };
        let mut files = Files::new(&evidence, Instant::now() + Duration::from_secs(5));
        let found = match_candidates(2, &target, 0, &mut files);
        assert_eq!(
            found
                .iter()
                .map(|c| (c.priority, c.order, block_range(&c.block).unwrap().0))
                .collect::<Vec<_>>(),
            [
                (2, (2, 0, 1), "first.rs".into()),
                (3, (2, 0, 1), "second.rs".into())
            ]
        );
    }
}
