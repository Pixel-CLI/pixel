// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `facts.relevance`: how widely the words of a task prompt occur in this
//! repository. The deterministic inputs a later gate uses to decide whether a
//! prompt is about the repository at all.
//!
//! [`relevance_from`] is the pure core: probe results and symbol hits in, a
//! [`Relevance`] out. [`relevance_for`] gathers those inputs from an index and
//! a graph, reusing the content probes a `targets` run already made;
//! [`relevance_on`] does the same in process for a reader that has no daemon.
//! Both routes return the same block for the same repository state.
//!
//! A keyword is counted three ways (files with a word-bounded content match,
//! files defining a symbol with that word in its name, files with that word in
//! a path), because a prompt can be about a repository through any of them. A
//! keyword the repository does not contain as typed (a French word) borrows
//! the counts of its first thesaurus synonym that it does contain.
//!
//! How selective a keyword is, [`keyword_weight`], is the one definition of a
//! rare word and of a ubiquitous one: co-files are ranked by the weights of the
//! keywords they match, and a gate built on these counts takes its weights from
//! the same function.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pixel_graph::GraphStore;
use pixel_graph::targets::{SymbolHit, symbol_hits};
use pixel_index::index::credential_path;
use pixel_index::indexset::IndexSet;
use pixel_proto::{CoFile, KeywordEvidence, Relevance};
use pixel_rank::{TaskQuery, semantic_expand, split_ident_words, tokenize_task};
use serde_json::{Value, json};

use crate::api::{ContentProbes, probe_cap, probe_content};

/// Synonym probes `relevance` may spend on the keywords the repository does
/// not contain as typed. The task's own keywords are always probed.
pub const RELEVANCE_EXPANSION_PROBES: usize = 6;

/// Co-files listed by weight: enough to start from, few enough to read at a
/// glance.
const COFILE_BY_WEIGHT: usize = 5;

/// Structural co-files listed whatever their weight, so a file the prompt
/// names by path or symbol is not crowded out by prose that merely repeats
/// its words.
const COFILE_STRUCTURAL: usize = 3;

/// The most one keyword weighs. A word in a handful of files is as telling as
/// a word in one, and a cap keeps a typo from outweighing three real words.
pub const IDF_CAP: f64 = 4.0;

/// A keyword in more than one file in this many is ubiquitous: it says
/// nothing about the repository.
const UBIQUITOUS_INVERSE_SHARE: usize = 4;

/// Decimal places kept in a co-file's weight, so the JSON is byte-stable.
const WEIGHT_SCALE: f64 = 1000.0;

/// Characters of a co-file's evidence line kept.
const COFILE_TEXT_CHARS: usize = 160;

/// Letters a word needs to take a plural in [`same_word`].
const PLURAL_MIN_STEM: usize = 3;

// ---------------------------------------------------------------------------
// weights
// ---------------------------------------------------------------------------

/// Whether a word found in `df` of `files_considered` files is ubiquitous:
/// strictly more than a quarter of them (`df` a quarter exactly is not).
pub fn is_ubiquitous(df: usize, files_considered: usize) -> bool {
    df.saturating_mul(UBIQUITOUS_INVERSE_SHARE) > files_considered
}

/// How much a keyword says about the repository: its inverse document
/// frequency `ln((n + 1) / (df + 1))`, at most [`IDF_CAP`].
///
/// 0 for a keyword that is [`is_ubiquitous`] or whose content probe
/// `truncated` (its `df` is then a prefix count, a lower bound: it may be in
/// most files). A keyword found in no file (`df` 0) weighs [`IDF_CAP`]: the
/// repository lacks the word, which a small repository's formula would
/// understate.
pub fn keyword_weight(df: usize, files_considered: usize, truncated: bool) -> f64 {
    if truncated || is_ubiquitous(df, files_considered) {
        return 0.0;
    }
    if df == 0 {
        return IDF_CAP;
    }
    let idf = ((files_considered + 1) as f64 / (df + 1) as f64).ln();
    idf.min(IDF_CAP)
}

/// The files a keyword is found in: its largest channel count. The channels
/// overlap and only the counts reach a reader, so this is a lower bound of
/// the union.
pub fn keyword_df(row: &KeywordEvidence) -> usize {
    row.content_files
        .max(row.symbol_files)
        .max(row.filename_files)
}

/// The weight of a keyword row in a repository of `files_considered` files.
pub fn row_weight(row: &KeywordEvidence, files_considered: usize) -> f64 {
    keyword_weight(keyword_df(row), files_considered, row.truncated)
}

/// `weight` kept to [`WEIGHT_SCALE`]'s decimals.
fn rounded(weight: f64) -> f64 {
    (weight * WEIGHT_SCALE).round() / WEIGHT_SCALE
}

// ---------------------------------------------------------------------------
// words
// ---------------------------------------------------------------------------

/// Whether two lowercase words are one word up to a plural: `setting` and
/// `settings`, `class` and `classes`, `entry` and `entries`.
fn same_word(a: &str, b: &str) -> bool {
    a == b || is_plural_of(a, b) || is_plural_of(b, a)
}

/// Whether `long` is `short` with an `s`, an `es` or (after a `y`) an `ies`.
/// A `short` under [`PLURAL_MIN_STEM`] letters never takes a plural: `bu` and
/// `bus`, `is` and `iss`, are not one word.
fn is_plural_of(short: &str, long: &str) -> bool {
    short.len() >= PLURAL_MIN_STEM
        && (matches!(long.strip_prefix(short), Some("s" | "es"))
            || short
                .strip_suffix('y')
                .is_some_and(|stem| long.strip_prefix(stem) == Some("ies")))
}

/// `word` and every other spelling [`same_word`] folds into it, so a graph
/// scan that matches words exactly still finds `setting` for `settings`.
fn plural_variants(word: &str) -> Vec<String> {
    let mut candidates = vec![format!("{word}s"), format!("{word}es")];
    if let Some(stem) = word.strip_suffix("ies") {
        candidates.push(format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix('y') {
        candidates.push(format!("{stem}ies"));
    }
    if let Some(stem) = word.strip_suffix("es") {
        candidates.push(stem.to_owned());
    }
    if let Some(stem) = word.strip_suffix('s') {
        candidates.push(stem.to_owned());
    }
    std::iter::once(word.to_owned())
        .chain(
            candidates
                .into_iter()
                .filter(|candidate| same_word(word, candidate)),
        )
        .collect()
}

/// The spellings of every word in `words`, once each.
fn spellings(words: &[String]) -> Vec<String> {
    words
        .iter()
        .flat_map(|word| plural_variants(word))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The part of `word` every spelling [`same_word`] folds it into shares: a
/// necessary condition, cheap to test on a whole path before splitting it.
fn word_stem(word: &str) -> &str {
    word.trim_end_matches(['s', 'e', 'i', 'y'])
}

/// The lowercase words of a repository-relative path: each directory name and
/// the file name without its extension (every Rust file would match `rs`),
/// split at `_`, `-`, `.` and camelCase.
fn path_words(path: &str) -> Vec<String> {
    let (dirs, file) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    };
    dirs.split('/')
        .chain([stem])
        .flat_map(split_ident_words)
        .collect()
}

/// For each of `forms`, the paths with that word in a directory or file name.
fn filename_files<'a>(all_paths: &'a [String], forms: &[&str]) -> Vec<BTreeSet<&'a str>> {
    let stems: Vec<&str> = forms.iter().map(|form| word_stem(form)).collect();
    let mut found: Vec<BTreeSet<&'a str>> = vec![BTreeSet::new(); forms.len()];
    for path in all_paths {
        let lower = path.to_ascii_lowercase();
        let candidates: Vec<usize> = (0..forms.len())
            .filter(|&at| lower.contains(stems[at]))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let words = path_words(path);
        for at in candidates {
            if words.iter().any(|word| same_word(forms[at], word)) {
                found[at].insert(path.as_str());
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// the pure core
// ---------------------------------------------------------------------------

/// The files one form (a keyword or a synonym) was found in, per channel.
/// Credential-shaped paths are left out of all three and kept in `hidden`:
/// like `search`, relevance never lists the name or a line of such a file.
#[derive(Debug, Default)]
struct Channels<'a> {
    content: BTreeSet<&'a str>,
    symbol: BTreeSet<&'a str>,
    filename: BTreeSet<&'a str>,
    hidden: BTreeSet<&'a str>,
}

impl Channels<'_> {
    fn any(&self) -> bool {
        !(self.content.is_empty() && self.symbol.is_empty() && self.filename.is_empty())
    }
}

/// `paths` without the credential-shaped ones, which are added to `hidden`.
fn visible<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    hidden: &mut BTreeSet<&'a str>,
) -> BTreeSet<&'a str> {
    paths
        .into_iter()
        .filter(|path| {
            let secret = credential_path(Path::new(path));
            if secret {
                hidden.insert(*path);
            }
            !secret
        })
        .collect()
}

/// Where each form occurs, looked up per form.
struct Lookup<'a> {
    probes: &'a ContentProbes,
    /// Each file with a symbol hit, and the words of the symbol names there.
    symbols: Vec<(&'a str, BTreeSet<String>)>,
    /// Form → the paths with that word in a directory or file name.
    filenames: BTreeMap<&'a str, BTreeSet<&'a str>>,
}

impl<'a> Lookup<'a> {
    fn new(
        probes: &'a ContentProbes,
        symbol_hits: &'a [SymbolHit],
        all_paths: &'a [String],
        forms: &[&'a str],
    ) -> Self {
        let symbols = symbol_hits
            .iter()
            .map(|hit| {
                let words = hit
                    .symbols
                    .iter()
                    .flat_map(|(symbol, _)| split_ident_words(&symbol.name))
                    .collect();
                (hit.path.as_str(), words)
            })
            .collect();
        let filenames = forms
            .iter()
            .copied()
            .zip(filename_files(all_paths, forms))
            .collect();
        Self {
            probes,
            symbols,
            filenames,
        }
    }

    fn channels(&self, form: &str) -> Channels<'a> {
        let probes: &'a ContentProbes = self.probes;
        let mut hidden = BTreeSet::new();
        let content = probes
            .hits
            .get(form)
            .into_iter()
            .flatten()
            .map(|(path, _)| path.as_str());
        let symbol = self
            .symbols
            .iter()
            .filter(|(_, words)| words.iter().any(|word| same_word(form, word)))
            .map(|(path, _)| *path);
        let filename = self.filenames.get(form).into_iter().flatten().copied();
        Channels {
            content: visible(content, &mut hidden),
            symbol: visible(symbol, &mut hidden),
            filename: visible(filename, &mut hidden),
            hidden,
        }
    }
}

/// One keyword's standing: the form whose channels stand for it.
struct Standing<'a> {
    form: &'a str,
    channels: Channels<'a>,
}

/// What a file is known by: the keywords it matches and whether one of them
/// matched its name or a symbol rather than only its text.
#[derive(Debug, Default)]
struct FileMatch {
    keywords: Vec<usize>,
    structural: bool,
}

/// A file that matched, with the weight of what it matched.
struct Candidate<'a> {
    path: &'a str,
    weight: f64,
    file: FileMatch,
}

/// Heaviest first, then structural before prose, then path order.
fn rank_order(a: &Candidate, b: &Candidate) -> Ordering {
    b.weight
        .total_cmp(&a.weight)
        .then_with(|| b.file.structural.cmp(&a.file.structural))
        .then_with(|| a.path.cmp(b.path))
}

/// The sentence for a task longer than the keyword list holds. The wording is
/// `pixel_rank::compute_targets`' own, so a `targets_facts` envelope that
/// already carries it takes this one as a duplicate and names it once.
fn keywords_cap(kept: usize) -> String {
    format!("task keywords truncated at {kept}; later task words contributed no signal")
}

/// The relevance block for `query` from the probes already run.
///
/// Per keyword: the files its content probe matched, the files defining a
/// symbol whose name has the word, the files with the word in a path. A
/// keyword with no match in any channel takes the counts of its first
/// *probed* thesaurus synonym that has one, and says which in
/// `via_expansion`. A co-file's weight is the sum of [`row_weight`] over the
/// keywords it matches, so a rare word counts and a ubiquitous or truncated
/// one does not; a file whose keywords all weigh nothing is dropped. The
/// co-files returned are the [`COFILE_BY_WEIGHT`] heaviest and the
/// [`COFILE_STRUCTURAL`] heaviest structural ones, heaviest first, structural
/// before prose on a tie, then by path.
///
/// `symbol_hits` is `None` when no graph answered. A file with more matching
/// symbols than the graph scan keeps (five) can lose a keyword that only its
/// later symbols carry; the keyword's content and filename counts still see
/// the file.
pub(crate) fn relevance_from(
    query: &TaskQuery,
    probes: &ContentProbes,
    symbol_hits: Option<&[SymbolHit]>,
    all_paths: &[String],
) -> Relevance {
    let keywords: Vec<&str> = query.keywords.iter().map(String::as_str).collect();
    let synonyms: Vec<Vec<&str>> = keywords
        .iter()
        .map(|keyword| {
            semantic_expand(keyword, query.language)
                .into_iter()
                .filter(|synonym| probes.probed.contains(*synonym))
                .collect()
        })
        .collect();
    let mut forms = keywords.clone();
    for synonym in synonyms.iter().flatten().copied() {
        if !forms.contains(&synonym) {
            forms.push(synonym);
        }
    }
    let lookup = Lookup::new(probes, symbol_hits.unwrap_or_default(), all_paths, &forms);

    let standings: Vec<Standing> = keywords
        .iter()
        .zip(&synonyms)
        .map(|(&keyword, alternatives)| {
            let own = lookup.channels(keyword);
            if own.any() {
                return Standing {
                    form: keyword,
                    channels: own,
                };
            }
            alternatives
                .iter()
                .map(|&synonym| Standing {
                    form: synonym,
                    channels: lookup.channels(synonym),
                })
                .find(|standing| standing.channels.any())
                .unwrap_or(Standing {
                    form: keyword,
                    channels: own,
                })
        })
        .collect();

    let rows: Vec<KeywordEvidence> = keywords
        .iter()
        .zip(&standings)
        .map(|(&keyword, standing)| KeywordEvidence {
            keyword: keyword.to_owned(),
            content_files: standing.channels.content.len(),
            truncated: probes.truncated.contains(standing.form),
            symbol_files: standing.channels.symbol.len(),
            filename_files: standing.channels.filename.len(),
            via_expansion: (standing.form != keyword).then(|| standing.form.to_owned()),
        })
        .collect();

    let weights: Vec<f64> = rows
        .iter()
        .map(|row| row_weight(row, all_paths.len()))
        .collect();
    let mut matched: BTreeMap<&str, FileMatch> = BTreeMap::new();
    for (at, standing) in standings.iter().enumerate() {
        let channels = &standing.channels;
        // A name or symbol only counts as structure when the word that
        // matched it says something: a directory called `src` is no sign
        // that a prompt is about this repository.
        let selective = weights[at] > 0.0;
        for (paths, structural) in [
            (&channels.content, false),
            (&channels.symbol, selective),
            (&channels.filename, selective),
        ] {
            for &path in paths {
                let file = matched.entry(path).or_default();
                if file.keywords.last() != Some(&at) {
                    file.keywords.push(at);
                }
                file.structural |= structural;
            }
        }
    }
    let mut candidates: Vec<Candidate> = matched
        .into_iter()
        .map(|(path, file)| Candidate {
            path,
            weight: rounded(file.keywords.iter().map(|&at| weights[at]).sum()),
            file,
        })
        .filter(|candidate| candidate.weight > 0.0)
        .collect();
    candidates.sort_by(rank_order);
    let eligible = candidates.len();
    let mut listed: Vec<Candidate> = Vec::new();
    let mut structural_seen = 0;
    for (at, candidate) in candidates.into_iter().enumerate() {
        if candidate.file.structural {
            structural_seen += 1;
        }
        let by_weight = at < COFILE_BY_WEIGHT;
        let best_structural = candidate.file.structural && structural_seen <= COFILE_STRUCTURAL;
        if by_weight || best_structural {
            listed.push(candidate);
        }
    }
    let cofiles = listed
        .iter()
        .map(|candidate| {
            let file = &candidate.file;
            let evidence = probes.lines.get(candidate.path).and_then(|lines| {
                lines.iter().find(|line| {
                    file.keywords
                        .iter()
                        .any(|&at| standings[at].form == line.keyword)
                })
            });
            CoFile {
                path: candidate.path.to_owned(),
                keywords: file
                    .keywords
                    .iter()
                    .map(|&at| keywords[at].to_owned())
                    .collect(),
                weight: candidate.weight,
                structural: file.structural,
                line: evidence.and_then(|line| u32::try_from(line.line).ok()),
                text: evidence.map(|line| {
                    line.text
                        .trim()
                        .chars()
                        .take(COFILE_TEXT_CHARS)
                        .collect::<String>()
                }),
            }
        })
        .collect();

    let mut caps = Vec::new();
    if query.keywords_truncated {
        caps.push(keywords_cap(query.keywords.len()));
    }
    for standing in &standings {
        let cap = probe_cap(standing.form);
        if probes.truncated.contains(standing.form) && !caps.contains(&cap) {
            caps.push(cap);
        }
    }
    let hidden: BTreeSet<&str> = standings
        .iter()
        .flat_map(|standing| standing.channels.hidden.iter().copied())
        .collect();
    if !hidden.is_empty() {
        caps.push(format!(
            "{} credential-shaped file(s) matched and are neither counted nor listed",
            hidden.len()
        ));
    }

    if eligible > listed.len() {
        caps.push(format!(
            "co-file list cut: {} of {eligible} matching files listed (the {COFILE_BY_WEIGHT} \
             heaviest and the {COFILE_STRUCTURAL} heaviest structural ones)",
            listed.len()
        ));
    }

    Relevance {
        files_considered: all_paths.len(),
        graph: symbol_hits.is_some(),
        keywords: rows,
        cofiles,
        caps,
    }
}

// ---------------------------------------------------------------------------
// gathering
// ---------------------------------------------------------------------------

/// Whether `keyword` occurs in the repository as typed, in any channel. Costs
/// a pass over the paths only when the content probe found nothing.
fn is_known(
    probes: &ContentProbes,
    symbol_hits: Option<&[SymbolHit]>,
    all_paths: &[String],
    keyword: &str,
) -> bool {
    let in_text = probes.hits.get(keyword).is_some_and(|files| {
        files
            .iter()
            .any(|(path, _)| !credential_path(Path::new(path)))
    });
    in_text
        || Lookup::new(
            probes,
            symbol_hits.unwrap_or_default(),
            all_paths,
            &[keyword],
        )
        .channels(keyword)
        .any()
}

/// The words quoted and comma-joined, for a cap sentence.
fn quoted(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| format!("'{word}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Gather what [`relevance_from`] needs and return its block.
///
/// `shared` holds the content probes a caller (`targets`) already ran: those
/// of the task's own keywords are reused, everything else is probed here, so
/// the block is the same whether or not a ranking ran first. A keyword with
/// no match as typed gets its thesaurus synonyms probed in thesaurus order,
/// until one has a content match, within [`RELEVANCE_EXPANSION_PROBES`] probes
/// for the whole task; the cap names the keywords left without.
///
/// # Errors
///
/// The graph cannot be read.
pub(crate) fn relevance_for(
    index: &IndexSet,
    graph: Option<&GraphStore>,
    query: &TaskQuery,
    all_paths: &[String],
    shared: Option<&ContentProbes>,
) -> Result<Relevance, String> {
    let mut probes = shared.map_or_else(ContentProbes::default, |ran| {
        ran.restricted_to(&query.keywords)
    });
    let unprobed: Vec<String> = query
        .keywords
        .iter()
        .filter(|keyword| !probes.probed.contains(*keyword))
        .cloned()
        .collect();
    probes.absorb(probe_content(index, &unprobed));

    let mut hits: Option<Vec<SymbolHit>> = match graph {
        Some(store) => {
            Some(symbol_hits(store, &spellings(&query.keywords), &[]).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    let mut spent = 0;
    let mut tried: Vec<String> = Vec::new();
    let mut left_without: Vec<&str> = Vec::new();
    for keyword in &query.keywords {
        let synonyms = semantic_expand(keyword, query.language);
        if synonyms.is_empty() || is_known(&probes, hits.as_deref(), all_paths, keyword) {
            continue;
        }
        for synonym in synonyms {
            if spent == RELEVANCE_EXPANSION_PROBES {
                left_without.push(keyword.as_str());
                break;
            }
            spent += 1;
            let synonym = synonym.to_owned();
            probes.absorb(probe_content(index, std::slice::from_ref(&synonym)));
            let found = probes.hits.contains_key(&synonym);
            tried.push(synonym);
            if found {
                break;
            }
        }
    }
    if let Some(store) = graph
        && !tried.is_empty()
    {
        let more = symbol_hits(store, &spellings(&tried), &[]).map_err(|e| e.to_string())?;
        hits.get_or_insert_with(Vec::new).extend(more);
    }

    let mut relevance = relevance_from(query, &probes, hits.as_deref(), all_paths);
    if !left_without.is_empty() {
        relevance.caps.push(format!(
            "synonym probes capped at {RELEVANCE_EXPANSION_PROBES}: not every synonym of {} was probed",
            quoted(&left_without)
        ));
    }
    Ok(relevance)
}

/// The relevance block of `task` over an index and, when there is one, a
/// graph: what `targets_facts` carries as `facts.relevance`, for a reader
/// that has no daemon. Without a graph `symbol_files` is 0 throughout and
/// the block says so (`graph: false`).
///
/// # Errors
///
/// The task has no searchable keyword, or the graph cannot be read.
pub fn relevance_on(
    index: &IndexSet,
    graph: Option<&GraphStore>,
    task: &str,
) -> Result<Relevance, String> {
    let query = tokenize_task(task)?;
    relevance_for(index, graph, &query, &index.paths(), None)
}

/// Put `relevance` in a `targets_facts` packet, and name its caps in the
/// packet's envelope, where `derive_epistemics` reads them: a cap the block
/// carries is a cap the response states.
pub(crate) fn attach(facts: &mut Value, relevance: &Relevance) -> Result<(), String> {
    let block = serde_json::to_value(relevance).map_err(|e| e.to_string())?;
    let Some(packet) = facts.as_object_mut() else {
        return Ok(());
    };
    packet.insert("relevance".into(), block);
    if relevance.caps.is_empty() {
        return Ok(());
    }
    let Some(envelope) = packet.get_mut("envelope").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    envelope.insert("lower_bound".into(), json!(true));
    let named = envelope.entry("caps").or_insert_with(|| json!([]));
    if let Some(caps) = named.as_array_mut() {
        for cap in &relevance.caps {
            if !caps.iter().any(|named| named.as_str() == Some(cap)) {
                caps.push(json!(cap));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ProbeLine, Request, Service};
    use pixel_graph::{SymbolKind, SymbolRow};
    use pixel_index::TrigramExtractor;
    use std::path::PathBuf;

    // ----- fixtures ---------------------------------------------------------

    fn query(task: &str) -> TaskQuery {
        tokenize_task(task).unwrap()
    }

    /// Paths that make a repository big enough for a word in a few files to be
    /// rare: `names` plus [`FILLER`] files that are about nothing.
    const FILLER: usize = 200;

    fn paths(names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(ToString::to_string)
            .chain((0..FILLER).map(|n| format!("filler/f{n:03}.txt")))
            .collect()
    }

    /// `count` distinct paths under `dir`.
    fn many(dir: &str, count: usize) -> Vec<String> {
        (0..count).map(|n| format!("{dir}/{n:03}.md")).collect()
    }

    /// Probes that matched each keyword once in each of its files.
    fn probed_files(hits: &[(&str, Vec<String>)]) -> ContentProbes {
        let mut probes = ContentProbes::default();
        for (keyword, files) in hits {
            probes.probed.insert((*keyword).to_owned());
            probes.hits.insert(
                (*keyword).to_owned(),
                files.iter().map(|file| (file.clone(), 1)).collect(),
            );
        }
        probes
    }

    /// Probes that matched `files` for each keyword, one match per file.
    fn probed(hits: &[(&str, &[&str])]) -> ContentProbes {
        let mut probes = ContentProbes::default();
        for (keyword, files) in hits {
            probes.probed.insert((*keyword).to_owned());
            if !files.is_empty() {
                probes.hits.insert(
                    (*keyword).to_owned(),
                    files.iter().map(|file| ((*file).to_owned(), 1)).collect(),
                );
            }
        }
        probes
    }

    fn line(keyword: &str, number: u64, text: &str) -> ProbeLine {
        ProbeLine {
            line: number,
            text: text.to_owned(),
            keyword: keyword.to_owned(),
        }
    }

    /// A graph hit for `path` whose symbols carry `names`.
    fn symbols_in(path: &str, names: &[&str]) -> SymbolHit {
        SymbolHit {
            path: path.to_owned(),
            symbols: names
                .iter()
                .map(|name| {
                    (
                        SymbolRow {
                            id: 1,
                            uid: format!("{path}#{name}#function"),
                            file_id: 1,
                            name: (*name).to_owned(),
                            qualified: (*name).to_owned(),
                            kind: SymbolKind::Function,
                            start_line: 1,
                            end_line: 2,
                            sig: String::new(),
                        },
                        (*name).to_owned(),
                    )
                })
                .collect(),
            distinct_keywords: 1,
            exact_name_hit: false,
        }
    }

    fn evidence(relevance: &Relevance, keyword: &str) -> KeywordEvidence {
        relevance
            .keywords
            .iter()
            .find(|row| row.keyword == keyword)
            .unwrap_or_else(|| panic!("no row for {keyword:?} in {:?}", relevance.keywords))
            .clone()
    }

    fn cofile_paths(relevance: &Relevance) -> Vec<&str> {
        relevance
            .cofiles
            .iter()
            .map(|cofile| cofile.path.as_str())
            .collect()
    }

    // ----- weights ----------------------------------------------------------

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn keyword_weight_should_be_the_inverse_document_frequency_below_the_cap() {
        assert!(close(keyword_weight(2, 100, false), (101.0_f64 / 3.0).ln()));
        assert!(close(
            keyword_weight(24, 100, false),
            (101.0_f64 / 25.0).ln()
        ));
        assert!(close(
            keyword_weight(25, 100, false),
            (101.0_f64 / 26.0).ln()
        ));
        assert!(
            close(keyword_weight(18, 1000, false), (1001.0_f64 / 19.0).ln()),
            "ln(1001 / 19) = 3.964 is under the cap"
        );
    }

    #[test]
    fn keyword_weight_should_stop_at_the_cap() {
        assert_eq!(IDF_CAP, 4.0);
        assert_eq!(
            keyword_weight(17, 1000, false),
            IDF_CAP,
            "ln(1001 / 18) = 4.018 is over it"
        );
        assert_eq!(keyword_weight(1, 1_000_000, false), IDF_CAP);
    }

    #[test]
    fn keyword_weight_should_give_a_word_found_nowhere_the_cap_whatever_the_size() {
        for files in [0, 3, 10, 1000] {
            assert_eq!(keyword_weight(0, files, false), IDF_CAP, "{files} files");
        }
    }

    #[test]
    fn keyword_weight_should_be_zero_when_the_probe_truncated() {
        assert_eq!(keyword_weight(1, 1000, true), 0.0);
        assert_eq!(keyword_weight(0, 1000, true), 0.0);
        assert!(keyword_weight(1, 1000, false) > 0.0);
    }

    #[test]
    fn keyword_weight_should_zero_a_word_only_once_it_is_ubiquitous() {
        assert!(keyword_weight(24, 100, false) > 1.0, "below a quarter");
        assert!(keyword_weight(25, 100, false) > 1.0, "a quarter exactly");
        assert_eq!(keyword_weight(26, 100, false), 0.0, "past a quarter");
    }

    #[test]
    fn is_ubiquitous_should_mean_strictly_more_than_a_quarter_of_the_files() {
        assert!(!is_ubiquitous(24, 100));
        assert!(!is_ubiquitous(25, 100), "df * 4 == n is not ubiquitous");
        assert!(is_ubiquitous(26, 100));
        assert!(!is_ubiquitous(0, 0));
        assert!(is_ubiquitous(1, 0));
        assert!(is_ubiquitous(usize::MAX, 100), "the product saturates");
    }

    #[test]
    fn keyword_df_should_be_the_largest_channel_count() {
        let row = |content, symbol, filename| KeywordEvidence {
            keyword: "x".into(),
            content_files: content,
            symbol_files: symbol,
            filename_files: filename,
            ..KeywordEvidence::default()
        };
        assert_eq!(keyword_df(&row(3, 1, 2)), 3);
        assert_eq!(keyword_df(&row(1, 4, 2)), 4);
        assert_eq!(keyword_df(&row(1, 2, 5)), 5);
        assert_eq!(keyword_df(&row(0, 0, 0)), 0);
    }

    #[test]
    fn row_weight_should_weigh_the_row_the_way_the_co_files_do() {
        let row = KeywordEvidence {
            keyword: "x".into(),
            content_files: 2,
            symbol_files: 5,
            ..KeywordEvidence::default()
        };
        assert!(close(row_weight(&row, 100), (101.0_f64 / 6.0).ln()));
        let truncated = KeywordEvidence {
            truncated: true,
            ..row.clone()
        };
        assert_eq!(row_weight(&truncated, 100), 0.0);
        let unknown = KeywordEvidence {
            keyword: "y".into(),
            ..KeywordEvidence::default()
        };
        assert_eq!(row_weight(&unknown, 100), IDF_CAP);
    }

    #[test]
    fn rounded_should_keep_three_decimals() {
        assert_eq!(rounded(1.2344), 1.234);
        assert_eq!(rounded(1.2346), 1.235);
        assert_eq!(rounded(4.0), 4.0);
        assert_eq!(rounded(0.0), 0.0);
    }

    // ----- words ------------------------------------------------------------

    #[test]
    fn same_word_should_fold_a_plural_and_nothing_else() {
        for (a, b) in [
            ("setting", "settings"),
            ("settings", "setting"),
            ("class", "classes"),
            ("entry", "entries"),
            ("entries", "entry"),
            ("hook", "hook"),
        ] {
            assert!(same_word(a, b), "{a} and {b} are one word");
        }
        for (a, b) in [
            ("set", "setting"),
            ("setting", "resettings"),
            ("hook", "hooks_x"),
            ("entry", "entrees"),
            ("claude", "cloud"),
            ("", "s"),
        ] {
            assert!(!same_word(a, b), "{a} and {b} are two words");
        }
    }

    #[test]
    fn same_word_should_refuse_a_stem_shorter_than_three_letters() {
        assert!(
            same_word("app", "apps"),
            "three letters is the shortest stem"
        );
        assert!(!same_word("ap", "aps"));
        assert!(!same_word("is", "iss"));
        assert!(same_word("spy", "spies"));
    }

    #[test]
    fn plural_variants_should_all_fold_into_the_word_they_came_from() {
        for word in [
            "settings", "setting", "entry", "entries", "class", "classes", "go",
        ] {
            for variant in plural_variants(word) {
                assert!(same_word(word, &variant), "{variant} folds into {word}");
            }
        }
        assert_eq!(
            plural_variants("setting"),
            ["setting", "settings", "settinges"]
        );
        assert_eq!(
            plural_variants("settings"),
            ["settings", "settingss", "settingses", "setting"]
        );
        assert_eq!(
            plural_variants("entry"),
            ["entry", "entrys", "entryes", "entries"]
        );
        assert_eq!(
            plural_variants("go"),
            ["go"],
            "a two-letter word has no spellings"
        );
    }

    #[test]
    fn spellings_should_list_every_variant_of_every_word_once() {
        let words = ["hook".to_owned(), "hooks".to_owned()];
        assert_eq!(
            spellings(&words),
            ["hook", "hookes", "hooks", "hookses", "hookss"]
        );
    }

    #[test]
    fn path_words_should_split_directories_and_drop_only_the_extension() {
        assert_eq!(
            path_words("crates/pixel-install/src/claude_settings.rs"),
            ["crates", "pixel", "install", "src", "claude", "settings"]
        );
        assert_eq!(path_words("Cargo.toml"), ["cargo"]);
        assert_eq!(path_words("src/HttpServer.ts"), ["src", "http", "server"]);
        assert_eq!(
            path_words(".gitignore"),
            ["gitignore"],
            "a dotfile keeps its name"
        );
        assert_eq!(path_words("README"), ["readme"]);
    }

    #[test]
    fn word_stem_should_cut_the_letters_a_plural_changes() {
        assert_eq!(word_stem("settings"), "setting");
        assert_eq!(word_stem("setting"), "setting");
        assert_eq!(word_stem("entries"), "entr");
        assert_eq!(word_stem("entry"), "entr");
        assert_eq!(word_stem("classes"), "cla");
        assert_eq!(word_stem("hook"), "hook");
        assert_eq!(word_stem("s"), "");
    }

    #[test]
    fn filename_files_should_find_a_path_for_every_spelling_of_the_form() {
        let all = paths(&[
            "src/class.rs",
            "src/entries/mod.rs",
            "src/entry.rs",
            "src/hooks.rs",
        ]);
        let found = filename_files(&all, &["classes", "entry", "hook"]);
        assert_eq!(found[0], BTreeSet::from(["src/class.rs"]));
        assert_eq!(
            found[1],
            BTreeSet::from(["src/entries/mod.rs", "src/entry.rs"])
        );
        assert_eq!(found[2], BTreeSet::from(["src/hooks.rs"]));
    }

    #[test]
    fn filename_files_should_match_a_word_of_a_path_up_to_a_plural() {
        let all = paths(&[
            "src/hook.rs",
            "src/hooks/mod.rs",
            "src/resettings.rs",
            "src/settings.rs",
            "src/entry.rs",
            "docs/hooked.md",
        ]);
        let found = filename_files(&all, &["hooks", "settings", "entries"]);
        assert_eq!(
            found[0],
            BTreeSet::from(["src/hook.rs", "src/hooks/mod.rs"])
        );
        assert_eq!(
            found[1],
            BTreeSet::from(["src/settings.rs"]),
            "resettings is another word"
        );
        assert_eq!(found[2], BTreeSet::from(["src/entry.rs"]));
    }

    #[test]
    fn filename_files_should_not_match_the_extension() {
        let all = paths(&["src/lib.rs", "src/rs/mod.rs"]);
        let found = filename_files(&all, &["rs"]);
        assert_eq!(found[0], BTreeSet::from(["src/rs/mod.rs"]));
    }

    // ----- relevance_from ---------------------------------------------------

    #[test]
    fn relevance_from_should_list_a_keyword_nothing_matches_with_zero_counts() {
        let relevance = relevance_from(
            &query("quantum flux capacitor"),
            &probed(&[("quantum", &[]), ("flux", &[]), ("capacitor", &[])]),
            Some(&[]),
            &paths(&["src/a.rs", "src/b.rs"]),
        );
        assert_eq!(
            relevance,
            Relevance {
                files_considered: FILLER + 2,
                graph: true,
                keywords: ["quantum", "flux", "capacitor"]
                    .map(|keyword| KeywordEvidence {
                        keyword: keyword.to_owned(),
                        ..KeywordEvidence::default()
                    })
                    .to_vec(),
                cofiles: Vec::new(),
                caps: Vec::new(),
            }
        );
    }

    #[test]
    fn relevance_from_should_count_each_channel_apart() {
        let all = paths(&[
            "src/install/mod.rs",
            "src/hooks.rs",
            "docs/a.md",
            "docs/b.md",
        ]);
        let relevance = relevance_from(
            &query("install hooks"),
            &probed(&[
                ("install", &["docs/a.md", "docs/b.md", "src/hooks.rs"]),
                ("hooks", &["docs/b.md"]),
            ]),
            Some(&[symbols_in("src/hooks.rs", &["install_hooks"])]),
            &all,
        );
        assert_eq!(
            evidence(&relevance, "install"),
            KeywordEvidence {
                keyword: "install".into(),
                content_files: 3,
                symbol_files: 1,
                filename_files: 1,
                ..KeywordEvidence::default()
            }
        );
        assert_eq!(
            evidence(&relevance, "hooks"),
            KeywordEvidence {
                keyword: "hooks".into(),
                content_files: 1,
                symbol_files: 1,
                filename_files: 1,
                ..KeywordEvidence::default()
            }
        );
        assert_eq!(relevance.files_considered, FILLER + 4);
    }

    #[test]
    fn relevance_from_should_fold_a_plural_in_symbol_names_and_paths() {
        // The prompt says `setting`; the code says `settings`.
        let all = paths(&["src/claude/settings.rs", "src/other.rs"]);
        let relevance = relevance_from(
            &query("claude setting"),
            &probed(&[("claude", &[]), ("setting", &[])]),
            Some(&[symbols_in("src/claude/settings.rs", &["merge_settings"])]),
            &all,
        );
        assert_eq!(
            evidence(&relevance, "setting"),
            KeywordEvidence {
                keyword: "setting".into(),
                symbol_files: 1,
                filename_files: 1,
                ..KeywordEvidence::default()
            }
        );
        assert_eq!(evidence(&relevance, "claude").filename_files, 1);
    }

    #[test]
    fn relevance_from_should_not_count_content_for_a_plural() {
        // The content probe is word-bounded: `setting` does not find `settings`.
        let relevance = relevance_from(
            &query("setting"),
            &probed(&[("setting", &[])]),
            None,
            &paths(&["src/a.rs"]),
        );
        assert_eq!(evidence(&relevance, "setting").content_files, 0);
    }

    #[test]
    fn relevance_from_should_report_no_graph_when_no_symbol_scan_ran() {
        let relevance = relevance_from(
            &query("install"),
            &probed(&[("install", &[])]),
            None,
            &paths(&["src/a.rs"]),
        );
        assert!(!relevance.graph);
        let relevance = relevance_from(
            &query("install"),
            &probed(&[("install", &[])]),
            Some(&[]),
            &paths(&["src/a.rs"]),
        );
        assert!(relevance.graph);
        assert_eq!(evidence(&relevance, "install").symbol_files, 0);
    }

    fn french() -> TaskQuery {
        let query = query("la connexion de l'utilisateur ne marche pas");
        assert_eq!(query.keywords, ["connexion", "utilisateur", "marche"]);
        query
    }

    #[test]
    fn relevance_from_should_fold_a_french_keyword_into_its_first_synonym_that_matches() {
        let probes = probed(&[
            ("connexion", &[]),
            ("utilisateur", &[]),
            ("marche", &[]),
            ("login", &["src/auth.rs", "src/web.rs"]),
            ("auth", &["src/auth.rs", "src/other.rs", "src/more.rs"]),
            ("user", &["src/user.rs"]),
        ]);
        let relevance = relevance_from(&french(), &probes, None, &paths(&["src/auth.rs"]));
        assert_eq!(
            evidence(&relevance, "connexion"),
            KeywordEvidence {
                keyword: "connexion".into(),
                content_files: 2,
                via_expansion: Some("login".into()),
                ..KeywordEvidence::default()
            },
            "thesaurus order: login is listed before auth"
        );
        assert_eq!(
            evidence(&relevance, "utilisateur").via_expansion.as_deref(),
            Some("user")
        );
        assert_eq!(
            evidence(&relevance, "marche"),
            KeywordEvidence {
                keyword: "marche".into(),
                ..KeywordEvidence::default()
            },
            "a word without a thesaurus entry stays unmatched"
        );
    }

    #[test]
    fn relevance_from_should_skip_a_synonym_with_no_match_for_the_next_one() {
        let probes = probed(&[
            ("connexion", &[]),
            ("login", &[]),
            ("auth", &["src/auth.rs"]),
        ]);
        let relevance =
            relevance_from(&query("connexion"), &probes, None, &paths(&["src/auth.rs"]));
        assert_eq!(
            evidence(&relevance, "connexion").via_expansion.as_deref(),
            Some("auth")
        );
    }

    #[test]
    fn relevance_from_should_not_consult_a_synonym_that_was_never_probed() {
        // `login` names a file, but no probe ran for it: it is not evidence.
        let relevance = relevance_from(
            &query("connexion"),
            &probed(&[("connexion", &[])]),
            None,
            &paths(&["src/login.rs"]),
        );
        let row = evidence(&relevance, "connexion");
        assert_eq!((row.filename_files, row.via_expansion), (0, None));
    }

    #[test]
    fn relevance_from_should_keep_a_keyword_the_repository_has_as_typed() {
        let probes = probed(&[
            ("auth", &["src/auth.rs"]),
            ("login", &["src/a.rs", "src/b.rs"]),
        ]);
        let relevance = relevance_from(&query("auth"), &probes, None, &paths(&["src/auth.rs"]));
        assert_eq!(
            evidence(&relevance, "auth"),
            KeywordEvidence {
                keyword: "auth".into(),
                content_files: 1,
                filename_files: 1,
                ..KeywordEvidence::default()
            },
            "a synonym someone else probed does not replace a word that matched"
        );
    }

    #[test]
    fn relevance_from_should_treat_a_filename_match_alone_as_a_match() {
        let probes = probed(&[("auth", &[]), ("login", &["src/a.rs"])]);
        let relevance = relevance_from(&query("auth"), &probes, None, &paths(&["src/auth.rs"]));
        assert_eq!(
            evidence(&relevance, "auth"),
            KeywordEvidence {
                keyword: "auth".into(),
                filename_files: 1,
                ..KeywordEvidence::default()
            }
        );
    }

    #[test]
    fn relevance_from_should_fold_a_synonym_that_matches_by_symbol_only() {
        let probes = probed(&[("connexion", &[]), ("login", &[])]);
        let relevance = relevance_from(
            &query("connexion"),
            &probes,
            Some(&[symbols_in("src/a.rs", &["do_login"])]),
            &paths(&["src/a.rs"]),
        );
        assert_eq!(
            evidence(&relevance, "connexion"),
            KeywordEvidence {
                keyword: "connexion".into(),
                symbol_files: 1,
                via_expansion: Some("login".into()),
                ..KeywordEvidence::default()
            }
        );
    }

    #[test]
    fn relevance_from_should_flag_truncation_of_the_form_that_stands_for_the_keyword() {
        let mut probes = probed(&[
            ("install", &["a.md"]),
            ("connexion", &[]),
            ("login", &["a.md"]),
            ("auth", &["a.md"]),
        ]);
        probes.truncated.insert("install".into());
        probes.truncated.insert("login".into());
        // `auth` truncated too, but `login` is the synonym that stands in.
        probes.truncated.insert("auth".into());
        let relevance = relevance_from(
            &query("install connexion"),
            &probes,
            None,
            &paths(&["a.md"]),
        );
        assert!(evidence(&relevance, "install").truncated);
        let folded = evidence(&relevance, "connexion");
        assert!(folded.truncated);
        assert_eq!(folded.via_expansion.as_deref(), Some("login"));
        assert_eq!(
            relevance.caps,
            [probe_cap("install"), probe_cap("login")],
            "one sentence per form that stands in, none for the unused synonym"
        );
    }

    #[test]
    fn relevance_from_should_not_flag_a_keyword_whose_probe_ran_to_the_end() {
        let mut probes = probed(&[("install", &["a.md"]), ("hooks", &["a.md"])]);
        probes.truncated.insert("hooks".into());
        let relevance = relevance_from(&query("install hooks"), &probes, None, &paths(&["a.md"]));
        assert!(!evidence(&relevance, "install").truncated);
        assert!(evidence(&relevance, "hooks").truncated);
        assert_eq!(relevance.caps, [probe_cap("hooks")]);
    }

    #[test]
    fn relevance_from_should_name_a_probe_cap_once_for_two_keywords_sharing_a_synonym() {
        let mut probes = probed(&[
            ("connexion", &[]),
            ("authentification", &[]),
            ("login", &["a.md"]),
        ]);
        probes.truncated.insert("login".into());
        let relevance = relevance_from(
            &query("connexion authentification"),
            &probes,
            None,
            &paths(&["a.md"]),
        );
        assert_eq!(relevance.caps, [probe_cap("login")]);
    }

    #[test]
    fn relevance_from_should_name_a_task_longer_than_the_keyword_list() {
        let task = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike";
        let long = query(task);
        assert!(long.keywords_truncated);
        assert_eq!(long.keywords.len(), 12);
        let relevance = relevance_from(&long, &ContentProbes::default(), None, &paths(&[]));
        assert_eq!(
            relevance.caps,
            ["task keywords truncated at 12; later task words contributed no signal"]
        );
        let short = relevance_from(
            &query("alpha bravo"),
            &ContentProbes::default(),
            None,
            &paths(&[]),
        );
        assert!(short.caps.is_empty());
    }

    #[test]
    fn relevance_from_should_rank_co_files_by_the_weight_of_what_they_match() {
        // Keywords: alpha beta gamma delta, in 4, 4, 2 and 1 of the files.
        let probes = probed(&[
            ("alpha", &["z.rs", "m.rs", "b.rs", "c.rs"]),
            ("beta", &["z.rs", "m.rs", "b.rs", "d.rs"]),
            ("gamma", &["z.rs", "m.rs"]),
            ("delta", &["z.rs"]),
        ]);
        let relevance = relevance_from(
            &query("alpha beta gamma delta"),
            &probes,
            None,
            &paths(&["z.rs"]),
        );
        assert_eq!(
            relevance
                .cofiles
                .iter()
                .map(|cofile| (cofile.path.as_str(), cofile.keywords.len()))
                .collect::<Vec<_>>(),
            [
                ("z.rs", 4),
                ("m.rs", 3),
                ("b.rs", 2),
                ("c.rs", 1),
                ("d.rs", 1)
            ],
            "the heaviest first, path ascending on a tie"
        );
        let weights: Vec<f64> = relevance.cofiles.iter().map(|c| c.weight).collect();
        assert!(
            weights.windows(2).all(|pair| pair[0] >= pair[1]),
            "{weights:?}"
        );
    }

    #[test]
    fn relevance_from_should_weigh_a_co_file_by_the_sum_of_its_keywords() {
        // 201 files; alpha is in 2 of them, beta in 5, gamma in 1.
        let probes = probed(&[
            ("alpha", &["a.md", "b.md"]),
            ("beta", &["a.md", "c.md", "d.md", "e.md", "f.md"]),
            ("gamma", &["a.md"]),
        ]);
        let relevance =
            relevance_from(&query("alpha beta gamma"), &probes, None, &paths(&["a.md"]));
        let alpha = (202.0_f64 / 3.0).ln().min(IDF_CAP);
        let beta = (202.0_f64 / 6.0).ln();
        let gamma = (202.0_f64 / 2.0).ln().min(IDF_CAP);
        let weight_of = |path: &str| {
            relevance
                .cofiles
                .iter()
                .find(|cofile| cofile.path == path)
                .unwrap()
                .weight
        };
        assert_eq!(weight_of("a.md"), rounded(alpha + beta + gamma));
        assert_eq!(weight_of("b.md"), rounded(alpha));
        assert_eq!(weight_of("c.md"), rounded(beta));
        assert_eq!(weight_of("a.md"), 11.517, "pinned: 4 + 3.517 + 4");
    }

    #[test]
    fn relevance_from_should_rank_a_structural_file_of_two_rare_words_above_prose_of_four_common_ones()
     {
        // 200 files. `alpha` and `beta` are each in one file, a symbol name;
        // the four common words are each in 50 files, one of them the same
        // guide.
        let common = |dir: &str| {
            let mut files = many(dir, 49);
            files.push("docs/guide.md".to_owned());
            files
        };
        let probes = probed_files(&[
            ("gamma", common("g")),
            ("delta", common("d")),
            ("epsilon", common("e")),
            ("zeta", common("z")),
        ]);
        let relevance = relevance_from(
            &query("alpha beta gamma delta epsilon zeta"),
            &probes,
            Some(&[symbols_in("src/alpha_beta.rs", &["alpha_beta_loader"])]),
            &paths(&[]),
        );
        let top: Vec<(&str, f64, bool)> = relevance
            .cofiles
            .iter()
            .map(|cofile| (cofile.path.as_str(), cofile.weight, cofile.structural))
            .collect();
        assert_eq!(
            top,
            [
                ("src/alpha_beta.rs", 8.0, true),
                ("docs/guide.md", 5.486, false),
                ("d/000.md", 1.371, false),
                ("d/001.md", 1.371, false),
                ("d/002.md", 1.371, false),
            ],
            "two rare words weigh 4 + 4; four common ones 4 x 1.371"
        );
    }

    #[test]
    fn relevance_from_should_list_a_structural_file_the_prose_outweighs() {
        // Six docs say both words; one source file defines `alpha`.
        let docs: Vec<String> = (0..6).map(|n| format!("docs/p{n}.md")).collect();
        let probes = probed_files(&[("alpha", docs.clone()), ("beta", docs)]);
        let relevance = relevance_from(
            &query("alpha beta"),
            &probes,
            Some(&[symbols_in("src/alpha.rs", &["alpha_tool"])]),
            &paths(&[]),
        );
        assert_eq!(
            cofile_paths(&relevance),
            [
                "docs/p0.md",
                "docs/p1.md",
                "docs/p2.md",
                "docs/p3.md",
                "docs/p4.md",
                "src/alpha.rs"
            ],
            "the five heaviest are prose; the structural file still comes with them"
        );
        let last = relevance.cofiles.last().unwrap();
        assert!(last.structural);
        assert_eq!(last.weight, 3.357, "alpha alone");
        assert_eq!(relevance.cofiles[0].weight, 6.715, "alpha and beta");
        assert_eq!(
            relevance.caps,
            [
                "co-file list cut: 6 of 7 matching files listed (the 5 heaviest and the 3 heaviest structural ones)"
            ]
        );
    }

    #[test]
    fn relevance_from_should_list_at_most_three_structural_files_beyond_the_heaviest_five() {
        let docs: Vec<String> = (0..6).map(|n| format!("docs/p{n}.md")).collect();
        let sources: Vec<SymbolHit> = (0..5)
            .map(|n| symbols_in(&format!("src/s{n}.rs"), &["alpha_tool"]))
            .collect();
        let probes = probed_files(&[("alpha", docs.clone()), ("beta", docs)]);
        let relevance = relevance_from(&query("alpha beta"), &probes, Some(&sources), &paths(&[]));
        assert_eq!(
            cofile_paths(&relevance),
            [
                "docs/p0.md",
                "docs/p1.md",
                "docs/p2.md",
                "docs/p3.md",
                "docs/p4.md",
                "src/s0.rs",
                "src/s1.rs",
                "src/s2.rs"
            ],
            "p5 and the fourth and fifth source files are cut"
        );
        assert_eq!(
            relevance.caps,
            [
                "co-file list cut: 8 of 11 matching files listed (the 5 heaviest and the 3 heaviest structural ones)"
            ]
        );
    }

    #[test]
    fn relevance_from_should_not_list_a_structural_file_twice_when_it_is_among_the_heaviest() {
        // Four structural files, all heavier than the prose: the fourth is
        // listed as one of the five heaviest, not as a structural extra.
        let sources: Vec<SymbolHit> = (0..4)
            .map(|n| symbols_in(&format!("src/s{n}.rs"), &["alpha_beta_tool"]))
            .collect();
        let docs: Vec<String> = (0..3).map(|n| format!("docs/p{n}.md")).collect();
        let probes = probed_files(&[("alpha", docs.clone())]);
        let relevance = relevance_from(&query("alpha beta"), &probes, Some(&sources), &paths(&[]));
        assert_eq!(
            cofile_paths(&relevance),
            [
                "src/s0.rs",
                "src/s1.rs",
                "src/s2.rs",
                "src/s3.rs",
                "docs/p0.md"
            ],
            "heavier structural files first, then the heaviest prose; 2 prose files are cut"
        );
        assert_eq!(relevance.cofiles.len(), 5);
    }

    #[test]
    fn relevance_from_should_put_structural_before_prose_and_prose_by_path_on_a_tie() {
        let probes = probed(&[("alpha", &["m.md", "b.md"])]);
        let relevance = relevance_from(
            &query("alpha"),
            &probes,
            Some(&[symbols_in("zz/tool.rs", &["alpha_tool"])]),
            &paths(&[]),
        );
        assert_eq!(cofile_paths(&relevance), ["zz/tool.rs", "b.md", "m.md"]);
        let weights: Vec<f64> = relevance.cofiles.iter().map(|c| c.weight).collect();
        assert_eq!(weights[0], weights[1], "the order is not the weight's");
        assert_eq!(weights[1], weights[2]);
    }

    #[test]
    fn relevance_from_should_drop_a_file_whose_every_keyword_is_ubiquitous() {
        // 200 files: alpha is in 50 (a quarter exactly, not ubiquitous), beta
        // in 51 (ubiquitous). Only `0both.md` is in both.
        let mut alpha = many("a", 49);
        alpha.push("0both.md".to_owned());
        let mut beta = many("b", 50);
        beta.push("0both.md".to_owned());
        let probes = probed_files(&[("alpha", alpha), ("beta", beta)]);
        let relevance = relevance_from(&query("alpha beta"), &probes, None, &paths(&[]));
        assert!(
            relevance
                .cofiles
                .iter()
                .all(|cofile| !cofile.path.starts_with("b/")),
            "a file only a ubiquitous word matched weighs nothing: {:?}",
            cofile_paths(&relevance)
        );
        let both = &relevance.cofiles[0];
        assert_eq!(both.path, "0both.md");
        assert_eq!(both.keywords, ["alpha", "beta"]);
        assert_eq!(both.weight, 1.371, "beta adds nothing");
        assert_eq!(relevance.cofiles[1].weight, 1.371);
    }

    #[test]
    fn relevance_from_should_drop_every_file_when_every_keyword_is_truncated() {
        let mut probes = probed(&[("alpha", &["a.md", "b.md"])]);
        probes.truncated.insert("alpha".into());
        let relevance = relevance_from(&query("alpha"), &probes, None, &paths(&[]));
        assert!(relevance.cofiles.is_empty(), "{:?}", relevance.cofiles);
        assert!(evidence(&relevance, "alpha").truncated);
    }

    #[test]
    fn relevance_from_should_not_call_a_ubiquitous_word_in_a_path_structure() {
        // `alpha` is in 60 of 202 files: ubiquitous. `alpha/tool.rs` is named
        // for it and says `beta` once; `beta/other.rs` is named for `beta`.
        let probes = probed_files(&[
            ("alpha", many("u", 60)),
            ("beta", vec!["alpha/tool.rs".to_owned()]),
        ]);
        let relevance = relevance_from(
            &query("alpha beta"),
            &probes,
            None,
            &paths(&["alpha/tool.rs", "beta/other.rs"]),
        );
        let by_path: Vec<(&str, bool)> = relevance
            .cofiles
            .iter()
            .map(|cofile| (cofile.path.as_str(), cofile.structural))
            .collect();
        assert_eq!(
            by_path,
            [("beta/other.rs", true), ("alpha/tool.rs", false)],
            "same weight; the name that matched a rare word is structure"
        );
    }

    #[test]
    fn relevance_from_should_list_five_by_weight_and_say_only_when_it_cut() {
        let six: Vec<String> = (0..6).map(|n| format!("d{n}.md")).collect();
        let cut = relevance_from(
            &query("alpha"),
            &probed_files(&[("alpha", six.clone())]),
            None,
            &paths(&[]),
        );
        assert_eq!(
            cofile_paths(&cut),
            ["d0.md", "d1.md", "d2.md", "d3.md", "d4.md"]
        );
        assert_eq!(
            cut.caps,
            [
                "co-file list cut: 5 of 6 matching files listed (the 5 heaviest and the 3 heaviest structural ones)"
            ]
        );
        let whole = relevance_from(
            &query("alpha"),
            &probed_files(&[("alpha", six[..5].to_vec())]),
            None,
            &paths(&[]),
        );
        assert_eq!(cofile_paths(&whole).len(), 5);
        assert!(
            whole.caps.is_empty(),
            "nothing was left out: {:?}",
            whole.caps
        );
    }

    #[test]
    fn relevance_from_should_keep_a_better_file_that_sorts_late() {
        let probes = probed(&[
            ("alpha", &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "z.rs"]),
            ("beta", &["z.rs"]),
        ]);
        let relevance = relevance_from(&query("alpha beta"), &probes, None, &paths(&[]));
        assert_eq!(
            cofile_paths(&relevance),
            ["z.rs", "a.rs", "b.rs", "c.rs", "d.rs"]
        );
    }

    #[test]
    fn relevance_from_should_return_the_same_block_twice() {
        let docs: Vec<String> = (0..6).map(|n| format!("docs/p{n}.md")).collect();
        let probes = probed_files(&[("alpha", docs.clone()), ("beta", docs)]);
        let hits = [symbols_in("src/alpha.rs", &["alpha_tool"])];
        let all = paths(&["src/alpha.rs"]);
        let first = relevance_from(&query("alpha beta"), &probes, Some(&hits), &all);
        let second = relevance_from(&query("alpha beta"), &probes, Some(&hits), &all);
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    #[test]
    fn relevance_from_should_list_the_keywords_of_a_co_file_in_task_order() {
        let probes = probed(&[("beta", &["a.rs"]), ("alpha", &["a.rs", "b.rs"])]);
        let relevance = relevance_from(&query("alpha beta"), &probes, None, &paths(&[]));
        assert_eq!(relevance.cofiles[0].keywords, ["alpha", "beta"]);
        assert_eq!(relevance.cofiles[1].keywords, ["alpha"]);
    }

    #[test]
    fn relevance_from_should_count_a_file_once_per_keyword_across_channels() {
        // `hooks.rs` meets `hooks` by content, by symbol and by name.
        let probes = probed(&[("hooks", &["src/hooks.rs"])]);
        let relevance = relevance_from(
            &query("hooks"),
            &probes,
            Some(&[symbols_in("src/hooks.rs", &["run_hooks"])]),
            &paths(&["src/hooks.rs"]),
        );
        assert_eq!(relevance.cofiles.len(), 1);
        assert_eq!(relevance.cofiles[0].keywords, ["hooks"]);
    }

    #[test]
    fn relevance_from_should_mark_a_file_structural_when_a_name_or_symbol_matched() {
        let probes = probed(&[("alpha", &["text.rs", "name/alpha.rs", "sym.rs"])]);
        let relevance = relevance_from(
            &query("alpha"),
            &probes,
            Some(&[symbols_in("sym.rs", &["alpha_thing"])]),
            &paths(&["name/alpha.rs", "text.rs", "sym.rs"]),
        );
        let structural = |path: &str| {
            relevance
                .cofiles
                .iter()
                .find(|cofile| cofile.path == path)
                .unwrap()
                .structural
        };
        assert!(
            !structural("text.rs"),
            "a text match alone is not structural"
        );
        assert!(structural("name/alpha.rs"));
        assert!(structural("sym.rs"));
    }

    #[test]
    fn relevance_from_should_mark_structural_a_file_only_a_path_word_matched() {
        let relevance = relevance_from(
            &query("alpha"),
            &probed(&[("alpha", &[])]),
            None,
            &paths(&["alpha/lib.rs"]),
        );
        assert_eq!(relevance.cofiles.len(), 1);
        assert!(relevance.cofiles[0].structural);
        assert_eq!(relevance.cofiles[0].line, None);
        assert_eq!(relevance.cofiles[0].text, None);
    }

    #[test]
    fn relevance_from_should_start_a_co_file_from_the_first_matching_line() {
        let mut probes = probed(&[("alpha", &["a.rs"]), ("beta", &["a.rs"])]);
        probes.lines.insert(
            "a.rs".into(),
            vec![
                line("alpha", 7, "  let alpha = 1;  "),
                line("beta", 9, "beta()"),
            ],
        );
        let relevance = relevance_from(&query("alpha beta"), &probes, None, &paths(&[]));
        let cofile = &relevance.cofiles[0];
        assert_eq!(cofile.line, Some(7));
        assert_eq!(cofile.text.as_deref(), Some("let alpha = 1;"), "trimmed");
    }

    #[test]
    fn relevance_from_should_take_a_line_through_the_synonym_that_stands_in() {
        let mut probes = probed(&[
            ("connexion", &[]),
            ("login", &["a.rs"]),
            ("auth", &["a.rs"]),
        ]);
        probes.lines.insert(
            "a.rs".into(),
            vec![line("auth", 3, "auth()"), line("login", 8, "login()")],
        );
        let relevance = relevance_from(&query("connexion"), &probes, None, &paths(&[]));
        assert_eq!(
            (
                relevance.cofiles[0].line,
                relevance.cofiles[0].text.as_deref()
            ),
            (Some(8), Some("login()")),
            "the line of a form that does not stand for the keyword is no evidence"
        );
    }

    #[test]
    fn relevance_from_should_cut_a_long_line_at_160_characters() {
        let at_cap = "x".repeat(COFILE_TEXT_CHARS);
        let over_cap = "y".repeat(COFILE_TEXT_CHARS + 1);
        let mut probes = probed(&[("alpha", &["a.rs", "b.rs"])]);
        probes
            .lines
            .insert("a.rs".into(), vec![line("alpha", 1, &at_cap)]);
        probes
            .lines
            .insert("b.rs".into(), vec![line("alpha", 1, &over_cap)]);
        let relevance = relevance_from(&query("alpha"), &probes, None, &paths(&[]));
        assert_eq!(relevance.cofiles[0].text.as_deref(), Some(at_cap.as_str()));
        assert_eq!(
            relevance.cofiles[1].text.as_deref(),
            Some("y".repeat(COFILE_TEXT_CHARS).as_str())
        );
    }

    #[test]
    fn relevance_from_should_cut_multibyte_text_on_a_character_boundary() {
        let accented = "é".repeat(COFILE_TEXT_CHARS + 5);
        let mut probes = probed(&[("alpha", &["a.rs"])]);
        probes
            .lines
            .insert("a.rs".into(), vec![line("alpha", 1, &accented)]);
        let relevance = relevance_from(&query("alpha"), &probes, None, &paths(&[]));
        assert_eq!(
            relevance.cofiles[0]
                .text
                .as_deref()
                .unwrap()
                .chars()
                .count(),
            COFILE_TEXT_CHARS
        );
    }

    #[test]
    fn relevance_from_should_drop_a_line_number_that_does_not_fit_u32() {
        let mut probes = probed(&[("alpha", &["a.rs", "b.rs"])]);
        probes.lines.insert(
            "a.rs".into(),
            vec![line("alpha", u64::from(u32::MAX), "last")],
        );
        probes.lines.insert(
            "b.rs".into(),
            vec![line("alpha", u64::from(u32::MAX) + 1, "past")],
        );
        let relevance = relevance_from(&query("alpha"), &probes, None, &paths(&[]));
        assert_eq!(relevance.cofiles[0].line, Some(u32::MAX));
        assert_eq!(relevance.cofiles[1].line, None);
        assert_eq!(relevance.cofiles[1].text.as_deref(), Some("past"));
    }

    #[test]
    fn relevance_from_should_neither_count_nor_list_a_credential_shaped_file() {
        let mut probes = probed(&[(
            "token",
            &[".env", "secrets/prod.yaml", "keys/id_rsa", "src/token.rs"],
        )]);
        probes
            .lines
            .insert(".env".into(), vec![line("token", 1, "TOKEN=hunter2")]);
        let relevance = relevance_from(
            &query("token"),
            &probes,
            Some(&[symbols_in("config/app.pem", &["token_loader"])]),
            &paths(&["secrets/token.txt", "src/token.rs"]),
        );
        let row = evidence(&relevance, "token");
        assert_eq!(
            (row.content_files, row.symbol_files, row.filename_files),
            (1, 0, 1),
            "only src/token.rs by content, and by name"
        );
        assert_eq!(cofile_paths(&relevance), ["src/token.rs"]);
        assert!(!format!("{relevance:?}").contains("hunter2"));
        assert_eq!(
            relevance.caps,
            ["5 credential-shaped file(s) matched and are neither counted nor listed"]
        );
    }

    #[test]
    fn relevance_from_should_count_a_hidden_file_once_across_keywords() {
        let probes = probed(&[("token", &[".env"]), ("secret", &[".env"])]);
        let relevance = relevance_from(&query("token secret"), &probes, None, &paths(&[]));
        assert_eq!(
            relevance.caps,
            ["1 credential-shaped file(s) matched and are neither counted nor listed"]
        );
    }

    #[test]
    fn relevance_from_should_fold_a_keyword_whose_only_matches_are_credential_files() {
        let probes = probed(&[("connexion", &[".env"]), ("login", &["src/login.rs"])]);
        let relevance = relevance_from(&query("connexion"), &probes, None, &paths(&[]));
        assert_eq!(
            evidence(&relevance, "connexion").via_expansion.as_deref(),
            Some("login")
        );
    }

    // ----- plumbing: probes and the packet -------------------------------------

    #[test]
    fn restricted_to_should_keep_only_the_named_keywords_everywhere() {
        let mut ran = probed(&[
            ("alpha", &["a.rs"]),
            ("beta", &["a.rs", "b.rs"]),
            ("gamma", &[]),
        ]);
        ran.truncated.insert("alpha".into());
        ran.truncated.insert("beta".into());
        ran.lines.insert(
            "a.rs".into(),
            vec![line("beta", 1, "b"), line("alpha", 2, "a")],
        );
        ran.lines.insert("b.rs".into(), vec![line("beta", 4, "bb")]);
        let kept = ran.restricted_to(&["alpha".into(), "gamma".into()]);
        assert_eq!(
            kept.probed,
            BTreeSet::from(["alpha".to_owned(), "gamma".to_owned()])
        );
        assert_eq!(kept.hits.keys().collect::<Vec<_>>(), ["alpha"]);
        assert_eq!(kept.truncated, BTreeSet::from(["alpha".to_owned()]));
        assert_eq!(kept.lines.len(), 1, "b.rs only had a beta line");
        assert_eq!(kept.lines["a.rs"], [line("alpha", 2, "a")]);
    }

    #[test]
    fn absorb_should_fold_probes_in_and_keep_two_lines_per_path() {
        let mut first = probed(&[("alpha", &["a.rs"])]);
        first
            .lines
            .insert("a.rs".into(), vec![line("alpha", 1, "one")]);
        let mut second = probed(&[("beta", &["a.rs", "b.rs"])]);
        second.truncated.insert("beta".into());
        second.lines.insert(
            "a.rs".into(),
            vec![line("beta", 2, "two"), line("beta", 3, "three")],
        );
        second
            .lines
            .insert("b.rs".into(), vec![line("beta", 4, "four")]);
        first.absorb(second);
        assert_eq!(first.hits.len(), 2);
        assert_eq!(first.probed.len(), 2);
        assert_eq!(first.truncated, BTreeSet::from(["beta".to_owned()]));
        assert_eq!(
            first.lines["a.rs"],
            [line("alpha", 1, "one"), line("beta", 2, "two")],
            "the earlier line stays, the room left takes one more"
        );
        assert_eq!(first.lines["b.rs"], [line("beta", 4, "four")]);

        let mut full = ContentProbes::default();
        full.lines.insert(
            "a.rs".into(),
            vec![line("alpha", 1, "one"), line("alpha", 2, "two")],
        );
        let mut late = probed(&[("beta", &["a.rs"])]);
        late.lines
            .insert("a.rs".into(), vec![line("beta", 9, "nine")]);
        full.absorb(late);
        assert_eq!(
            full.lines["a.rs"].len(),
            2,
            "a path with its two lines takes no third"
        );
    }

    #[test]
    fn caps_should_name_the_truncated_probes_in_probe_order() {
        let mut probes = probed(&[
            ("alpha", &["a.rs"]),
            ("beta", &["a.rs"]),
            ("gamma", &["a.rs"]),
        ]);
        probes.truncated.insert("gamma".into());
        probes.truncated.insert("alpha".into());
        let order = ["gamma".to_owned(), "beta".to_owned(), "alpha".to_owned()];
        assert_eq!(
            probes.caps(&order),
            [probe_cap("gamma"), probe_cap("alpha")]
        );
        assert_eq!(
            probe_cap("alpha"),
            "content probe truncated at 1000 matches for keyword 'alpha'; \
             files beyond the cap carry no content signal"
        );
    }

    fn packet() -> Value {
        json!({
            "targets": [],
            "envelope": {"lower_bound": false, "caps": ["existing cap"]},
        })
    }

    #[test]
    fn attach_should_add_the_block_and_name_new_caps_in_the_envelope() {
        let mut facts = packet();
        let relevance = Relevance {
            files_considered: 4,
            caps: vec!["existing cap".into(), "new cap".into()],
            ..Relevance::default()
        };
        attach(&mut facts, &relevance).unwrap();
        assert_eq!(
            facts["relevance"],
            json!({"files_considered": 4, "graph": false, "caps": ["existing cap", "new cap"]})
        );
        assert_eq!(
            facts["envelope"],
            json!({"lower_bound": true, "caps": ["existing cap", "new cap"]}),
            "a cap already named stays once; a new one forces lower_bound"
        );
    }

    #[test]
    fn attach_should_leave_the_envelope_alone_when_the_block_fired_no_cap() {
        let mut facts = packet();
        attach(&mut facts, &Relevance::default()).unwrap();
        assert_eq!(facts["envelope"], packet()["envelope"]);
        assert_eq!(
            facts["relevance"],
            json!({"files_considered": 0, "graph": false})
        );
    }

    #[test]
    fn attach_should_create_the_cap_list_when_the_envelope_has_none() {
        let mut facts = json!({"envelope": {"lower_bound": false}});
        let relevance = Relevance {
            caps: vec!["only cap".into()],
            ..Relevance::default()
        };
        attach(&mut facts, &relevance).unwrap();
        assert_eq!(
            facts["envelope"],
            json!({"lower_bound": true, "caps": ["only cap"]})
        );
    }

    #[test]
    fn attach_should_ignore_a_packet_that_is_not_an_object() {
        let mut facts = json!([1, 2]);
        attach(&mut facts, &Relevance::default()).unwrap();
        assert_eq!(facts, json!([1, 2]));
    }

    #[test]
    fn quoted_should_quote_and_join() {
        assert_eq!(quoted(&["a", "b"]), "'a', 'b'");
        assert_eq!(quoted(&[]), "");
    }

    // ----- a real repository -----------------------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Files added to every fixture repository so a word in a few of them is
    /// rare, not ubiquitous.
    const REPO_FILLER: usize = 40;

    fn repo(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pixel-relevance-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let filler: Vec<(String, &str)> = (0..REPO_FILLER)
            .map(|n| (format!("filler/f{n:02}.txt"), "nothing here\n"))
            .collect();
        let named = files.iter().map(|(path, body)| ((*path).to_owned(), *body));
        for (path, body) in named.chain(filler) {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "fixture"]);
        root.canonicalize().unwrap()
    }

    /// The text index of `root` and, once `Service` built it, the graph.
    fn open(root: &Path) -> (IndexSet, GraphStore) {
        let mut service = Service::open(root).unwrap();
        let built = service.handle(Request::Targets {
            task: "build the graph".into(),
            limit: Some(1),
            max_tier: None,
            precision: false,
            regions: false,
        });
        assert!(built.ok, "fixture graph build: {built:?}");
        let graph = GraphStore::open_read_only(&service.graph_db_path()).unwrap();
        (
            IndexSet::open_or_build(root, Box::new(TrigramExtractor)).unwrap(),
            graph,
        )
    }

    const INSTALL_REPO: &[(&str, &str)] = &[
        (
            "crates/install/src/claude_settings.rs",
            "// install must handle existing settings\n\
             pub fn merge_claude_settings(existing: &str) -> String {\n    existing.to_string()\n}\n",
        ),
        (
            "crates/install/src/hooks.rs",
            "pub fn install_hooks() {}\n// settings for hooks\n",
        ),
        (
            "docs/manual-setup.md",
            "To install by hand, edit the settings file.\n",
        ),
        ("src/unrelated.rs", "pub fn pad_left() {}\n"),
        (".env", "install=1\nTOKEN=hunter2 install\n"),
    ];

    #[test]
    fn probe_content_should_match_whole_words_and_keep_the_first_two_lines_per_path() {
        let root = repo(
            "probe",
            &[
                (
                    "a.rs",
                    "// alpha one\n// beta\n// alpha two\n// alpha three\n",
                ),
                ("b.rs", "// alphabet and alpha_beta, not the word\n"),
            ],
        );
        let (index, _) = open(&root);
        let probes = probe_content(
            &index,
            &["alpha".to_owned(), "beta".to_owned(), "zeta".to_owned()],
        );
        assert_eq!(
            probes.hits["alpha"],
            [("a.rs".to_owned(), 3)],
            "alphabet and alpha_beta are other words"
        );
        assert_eq!(probes.hits["beta"], [("a.rs".to_owned(), 1)]);
        assert!(!probes.hits.contains_key("zeta"));
        assert_eq!(
            probes.probed,
            BTreeSet::from(["alpha".to_owned(), "beta".to_owned(), "zeta".to_owned()]),
            "a probe without a match still ran"
        );
        assert!(probes.truncated.is_empty());
        assert_eq!(
            probes.lines["a.rs"],
            [
                line("alpha", 1, "// alpha one"),
                line("alpha", 3, "// alpha two")
            ],
            "two lines per path, in probe order"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_content_should_stop_at_the_match_cap_and_say_so() {
        // 12 files x 100 lines = 1,200 matches, past CONTENT_PROBE_LIMIT.
        let files: Vec<(String, String)> = (0..12)
            .map(|file| {
                (
                    format!("f{file:02}.rs"),
                    (0..100).map(|n| format!("// needle {n}\n")).collect(),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect();
        let root = repo("probe-cap", &borrowed);
        let (index, _) = open(&root);
        let probes = probe_content(&index, &["needle".to_owned()]);
        assert_eq!(probes.truncated, BTreeSet::from(["needle".to_owned()]));
        let matched: u32 = probes.hits["needle"].iter().map(|(_, count)| count).sum();
        assert_eq!(matched, 1000, "exactly the cap, not one more");
        assert_eq!(
            probes.hits["needle"].len(),
            10,
            "a path-ordered prefix of the files"
        );
        assert_eq!(probes.caps(&["needle".to_owned()]), [probe_cap("needle")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_count_every_channel_of_a_real_repository() {
        let root = repo("on-graph", INSTALL_REPO);
        let (index, graph) = open(&root);
        let relevance = relevance_on(
            &index,
            Some(&graph),
            "how does install handle existing Claude settings",
        )
        .unwrap();

        assert_eq!(
            relevance.files_considered,
            REPO_FILLER + 5,
            "every indexed file, .env included"
        );
        assert!(relevance.graph);
        let rows: Vec<(&str, usize, usize, usize)> = relevance
            .keywords
            .iter()
            .map(|row| {
                (
                    row.keyword.as_str(),
                    row.content_files,
                    row.symbol_files,
                    row.filename_files,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("does", 0, 0, 0),
                ("install", 2, 1, 2),
                ("handle", 1, 0, 0),
                ("existing", 1, 0, 0),
                ("claude", 0, 1, 1),
                ("settings", 3, 1, 1),
            ]
        );
        assert!(relevance.keywords.iter().all(|row| !row.truncated));
        assert!(
            relevance
                .keywords
                .iter()
                .all(|row| row.via_expansion.is_none())
        );
        assert_eq!(
            cofile_paths(&relevance),
            [
                "crates/install/src/claude_settings.rs",
                "crates/install/src/hooks.rs",
                "docs/manual-setup.md",
            ],
            "heaviest first, structural before prose on a tie; .env is neither listed nor counted"
        );
        assert_eq!(
            relevance
                .cofiles
                .iter()
                .map(|cofile| cofile.weight)
                .collect::<Vec<_>>(),
            [14.579, 5.172, 5.172],
            "install 2, handle 1, existing 1, claude 1, settings 3 files of 45"
        );
        let best = &relevance.cofiles[0];
        assert_eq!(
            best.keywords,
            ["install", "handle", "existing", "claude", "settings"]
        );
        assert!(best.structural);
        assert_eq!(best.line, Some(1));
        assert_eq!(
            best.text.as_deref(),
            Some("// install must handle existing settings")
        );
        assert!(
            relevance.cofiles[2..]
                .iter()
                .all(|cofile| !cofile.structural || cofile.path.contains("hooks"))
        );
        assert_eq!(
            relevance.caps,
            ["1 credential-shaped file(s) matched and are neither counted nor listed"]
        );
        assert!(
            !serde_json::to_string(&relevance)
                .unwrap()
                .contains("hunter2")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_answer_without_a_graph_and_say_so() {
        let root = repo("on-nograph", INSTALL_REPO);
        let (index, _graph) = open(&root);
        let relevance = relevance_on(&index, None, "install claude settings").unwrap();
        assert!(!relevance.graph);
        assert!(relevance.keywords.iter().all(|row| row.symbol_files == 0));
        assert_eq!(evidence(&relevance, "claude").filename_files, 1);
        assert_eq!(evidence(&relevance, "install").filename_files, 2);
        let hooks = relevance
            .cofiles
            .iter()
            .find(|cofile| cofile.path == "crates/install/src/hooks.rs")
            .unwrap();
        assert!(hooks.structural, "its directory is named install");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_fold_a_plural_the_graph_scan_matches_exactly() {
        let root = repo("on-plural", INSTALL_REPO);
        let (index, graph) = open(&root);
        let relevance = relevance_on(&index, Some(&graph), "claude setting").unwrap();
        assert_eq!(
            evidence(&relevance, "setting").symbol_files,
            1,
            "merge_claude_settings"
        );
        assert_eq!(evidence(&relevance, "setting").filename_files, 1);
        assert_eq!(evidence(&relevance, "setting").content_files, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    const LOGIN_REPO: &[(&str, &str)] = &[
        ("src/auth.rs", "// login flow\npub fn check() {}\n"),
        ("src/other.rs", "pub fn pad_left() {}\n"),
    ];

    #[test]
    fn relevance_on_should_borrow_the_counts_of_a_synonym_for_a_french_keyword() {
        let root = repo("on-french", LOGIN_REPO);
        let (index, graph) = open(&root);
        let relevance = relevance_on(&index, Some(&graph), "la connexion ne marche pas").unwrap();
        assert_eq!(
            evidence(&relevance, "connexion"),
            KeywordEvidence {
                keyword: "connexion".into(),
                content_files: 1,
                via_expansion: Some("login".into()),
                ..KeywordEvidence::default()
            }
        );
        assert_eq!(cofile_paths(&relevance), ["src/auth.rs"]);
        assert_eq!(relevance.cofiles[0].keywords, ["connexion"]);
        assert_eq!(relevance.cofiles[0].line, Some(1));
        assert!(relevance.caps.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_stop_probing_synonyms_at_the_first_content_match() {
        // `connexion` has four synonyms and `utilisateur` three; probing all
        // of them would be seven probes. The first synonym of each matches,
        // so two probes suffice and the cap of six never fires.
        let root = repo(
            "on-first",
            &[("src/auth.rs", "// login flow\n// user record\n")],
        );
        let (index, graph) = open(&root);
        let relevance =
            relevance_on(&index, Some(&graph), "connexion utilisateur ne marche pas").unwrap();
        assert_eq!(
            evidence(&relevance, "connexion").via_expansion.as_deref(),
            Some("login")
        );
        assert_eq!(
            evidence(&relevance, "utilisateur").via_expansion.as_deref(),
            Some("user")
        );
        assert!(relevance.caps.is_empty(), "{:?}", relevance.caps);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_cap_synonym_probes_and_name_the_keywords_it_left_out() {
        let root = repo("on-budget", LOGIN_REPO);
        let (index, graph) = open(&root);
        // corriger: fix repair patch resolve; ajouter: add create insert;
        // supprimer: remove delete destroy. None occurs in the repository.
        let relevance = relevance_on(&index, Some(&graph), "corriger ajouter supprimer").unwrap();
        assert!(
            relevance
                .keywords
                .iter()
                .all(|row| row.via_expansion.is_none()),
            "no synonym matched anything: {:?}",
            relevance.keywords
        );
        assert_eq!(
            relevance.caps,
            ["synonym probes capped at 6: not every synonym of 'ajouter', 'supprimer' was probed"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_spend_no_synonym_probe_on_a_keyword_the_repository_has() {
        // `session` has a thesaurus entry; the repository has the word, so
        // no probe is spent and the French keyword still gets its turn.
        let root = repo(
            "on-spend",
            &[
                ("src/session.rs", "// session\n"),
                ("src/a.rs", "// login\n"),
            ],
        );
        let (index, graph) = open(&root);
        let relevance =
            relevance_on(&index, Some(&graph), "session connexion ne marche pas").unwrap();
        assert_eq!(evidence(&relevance, "session").via_expansion, None);
        assert_eq!(
            evidence(&relevance, "connexion").via_expansion.as_deref(),
            Some("login")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_spend_no_synonym_probe_on_a_keyword_known_by_text_or_by_name() {
        // `rust` and `python` are in the repository only as directory names,
        // `java` only in a text. Their thesaurus entries are 4 + 4 + 3 probes
        // that would use up the cap of six before `connexion` is reached.
        let root = repo(
            "on-known",
            &[
                ("src/rust/mod.rs", "pub fn first() {}\n"),
                ("src/python/mod.rs", "pub fn second() {}\n"),
                ("notes/n.md", "java notes\n"),
                ("src/auth.rs", "// login flow\n"),
            ],
        );
        let (index, graph) = open(&root);
        let relevance = relevance_on(
            &index,
            Some(&graph),
            "rust python java connexion ne marche pas",
        )
        .unwrap();
        for known in ["rust", "python", "java"] {
            assert_eq!(evidence(&relevance, known).via_expansion, None, "{known}");
        }
        assert_eq!(evidence(&relevance, "rust").filename_files, 1);
        assert_eq!(evidence(&relevance, "java").content_files, 1);
        assert_eq!(
            evidence(&relevance, "connexion").via_expansion.as_deref(),
            Some("login")
        );
        assert!(relevance.caps.is_empty(), "{:?}", relevance.caps);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_borrow_a_synonym_that_only_a_symbol_name_contains() {
        // `login` never appears as a word of a text, so no content probe
        // finds it: only the graph scan over the synonyms does.
        let root = repo("on-symbol", &[("src/a.rs", "pub fn do_login() {}\n")]);
        let (index, graph) = open(&root);
        let relevance = relevance_on(&index, Some(&graph), "la connexion ne marche pas").unwrap();
        assert_eq!(
            evidence(&relevance, "connexion"),
            KeywordEvidence {
                keyword: "connexion".into(),
                symbol_files: 1,
                via_expansion: Some("login".into()),
                ..KeywordEvidence::default()
            }
        );
        assert_eq!(cofile_paths(&relevance), ["src/a.rs"]);
        let without = relevance_on(&index, None, "la connexion ne marche pas").unwrap();
        assert_eq!(evidence(&without, "connexion").via_expansion, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_for_should_return_the_same_block_whatever_probes_a_caller_ran() {
        let root = repo("for-shared", LOGIN_REPO);
        let (index, graph) = open(&root);
        let query = query("la connexion ne marche pas");
        let all = index.paths();
        let alone = relevance_for(&index, Some(&graph), &query, &all, None).unwrap();

        // What `targets` probes: the keywords, then synonyms in its own order.
        let mut ran = probe_content(
            &index,
            &[
                "connexion".to_owned(),
                "marche".to_owned(),
                "session".to_owned(),
                "auth".to_owned(),
                "login".to_owned(),
            ],
        );
        let shared = relevance_for(&index, Some(&graph), &query, &all, Some(&ran)).unwrap();
        assert_eq!(shared, alone);

        // A caller that probed nothing the task needs gets the same block.
        ran = ContentProbes::default();
        assert_eq!(
            relevance_for(&index, Some(&graph), &query, &all, Some(&ran)).unwrap(),
            alone
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relevance_on_should_refuse_a_task_with_no_searchable_word() {
        let root = repo("on-empty", LOGIN_REPO);
        let (index, _) = open(&root);
        assert_eq!(
            relevance_on(&index, None, "to be or not").unwrap_err(),
            "task description yields no searchable keywords"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
