// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Plain-word leads when the meaning search cannot answer: BM25 over each
//! indexed file's path and its symbols' names and signatures, pixel-mvp's
//! ADR-0030 with a test file's score halved (ADR-0031), moved into this
//! brief under ADR-0036 of the pixel-mvp research repository.
//!
//! The meaning search needs the daemon and an embedding model on disk; a
//! machine that never downloaded the model, or a prompt that finds no daemon,
//! gets none of its leads, and the brief's files were then the relevance
//! co-files alone. These leads take the meaning leads' place in the fusion
//! (`chain::fuse_with`) and nowhere else: the gate, the excerpts and the
//! question-kind seeds still read the meaning leads only, so a brief whose
//! meaning search answered is the same with or without this module.
//!
//! The terms are pixel-mvp's, read from its vendored copy (`mvp-model`, never
//! edited here): `mvp_model::terms` splits and stems an identifier, a path and
//! the prompt alike, so both sides meet on one form. The documents are built
//! from this repository's own graph (`files`, `symbols`) on each call, read
//! only: pixel-mvp stores its postings at publication, which v1's graph does
//! not, and on v1's own repository (543 files, 9 066 symbols) the whole
//! corpus is read and scored in a few milliseconds.
//!
//! Measured on `eval/brief-gate/` before it went on
//! (`docs/bench/brief-gate.md`, "Plain-word leads without the meaning search").

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use mvp_model::{sub_words, terms};
use pixel_graph::GraphStore;

use super::chain::MeaningHit;

/// The switch: `auto`, the default, ranks plain words when the meaning search
/// did not answer; `0`, `false` or `off` never does.
pub(crate) const WORDS_ENV: &str = "PIXEL_BRIEF_BM25";

/// BM25's constants (pixel-mvp's ADR-0030): the usual k1 and b.
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// The distinct terms of the prompt the best file must hold for any lead:
/// fewer is a prompt the repository's names do not speak about
/// (pixel-mvp's ADR-0030 §5).
const MIN_TERMS: usize = 2;
/// A test file's share of its score (pixel-mvp's ADR-0031): a test repeats
/// the names of the code it tests in a shorter document and would outrank it.
const TEST_SHARE: f64 = 0.5;
/// Threads one call counts the documents on.
const THREADS: usize = 4;
/// Symbol rows read at most: a bound on the work in a repository far larger
/// than the ones measured.
const MAX_SYMBOLS: i64 = 400_000;

/// Whether [`WORDS_ENV`]'s value turns the leads on.
pub(crate) fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// One symbol of a file, as the graph has it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SymbolRow {
    pub(crate) name: String,
    pub(crate) signature: String,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
}

/// One indexed file: its path and its symbols.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FileRow {
    pub(crate) path: String,
    pub(crate) symbols: Vec<SymbolRow>,
}

/// At most `limit` leads for `typed`, best first, read from the graph.
pub(crate) fn leads(
    store: &GraphStore,
    typed: &str,
    limit: usize,
    deadline: Instant,
) -> Result<Vec<MeaningHit>, String> {
    let wanted = prompt_terms(typed);
    if wanted.len() < MIN_TERMS {
        return Ok(Vec::new());
    }
    let files = read_files(store)?;
    if Instant::now() >= deadline {
        return Err("words: the window ended while reading the graph".to_string());
    }
    Ok(rank(&files, &wanted, limit))
}

/// The prompt's distinct terms, in order.
fn prompt_terms(typed: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    terms(typed)
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

/// Every indexed file with its symbols, in path order.
fn read_files(store: &GraphStore) -> Result<Vec<FileRow>, String> {
    let sql = |error: rusqlite::Error| format!("words: {error}");
    let conn = store.conn();
    let mut files: Vec<FileRow> = Vec::new();
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut statement = conn
        .prepare("SELECT id, path FROM files ORDER BY path")
        .map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql)?;
    for row in rows {
        let (id, path) = row.map_err(sql)?;
        index.insert(id, files.len());
        files.push(FileRow {
            path,
            symbols: Vec::new(),
        });
    }
    let mut statement = conn
        .prepare(
            "SELECT file_id, name, sig, start_line, end_line FROM symbols
             ORDER BY file_id, start_line LIMIT ?1",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([MAX_SYMBOLS], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                SymbolRow {
                    name: row.get(1)?,
                    signature: row.get(2)?,
                    start_line: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    end_line: u32::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                },
            ))
        })
        .map_err(sql)?;
    for row in rows {
        let (file_id, symbol) = row.map_err(sql)?;
        if let Some(&at) = index.get(&file_id) {
            files[at].symbols.push(symbol);
        }
    }
    Ok(files)
}

/// A file's document as BM25 reads it: how often each wanted term occurs in
/// it, and how many terms it holds in all.
struct Document {
    counts: BTreeMap<usize, u32>,
    length: usize,
}

/// What one sub-word is to BM25: no term (a stop word, a single
/// character), or a term, wanted at an index of the prompt's terms or not.
type Word = Option<Option<usize>>;

/// The sub-words seen so far and what each is. `terms(text)` is the term of
/// each of `text`'s sub-words in turn (a sub-word is lower case and splits no
/// further), so a word is stemmed once per call, not once per occurrence: a
/// repository repeats few words many times.
struct Words<'a> {
    wanted: &'a HashMap<&'a str, usize>,
    seen: HashMap<String, Word>,
}

impl Words<'_> {
    /// Count the wanted terms of one text into `doc`.
    fn note(&mut self, doc: &mut Document, text: &str) {
        for word in sub_words(text) {
            let known = match self.seen.get(&word) {
                Some(&known) => known,
                None => {
                    let known = terms(&word)
                        .first()
                        .map(|term| self.wanted.get(term.as_str()).copied());
                    self.seen.insert(word, known);
                    known
                }
            };
            if let Some(wanted) = known {
                doc.length += 1;
                if let Some(at) = wanted {
                    *doc.counts.entry(at).or_insert(0) += 1;
                }
            }
        }
    }
}

/// Each file's document, in the order of `files`, counted on up to
/// [`THREADS`] threads: the corpus is read whole on every call.
fn documents(files: &[FileRow], wanted: &HashMap<&str, usize>) -> Vec<Document> {
    if files.is_empty() {
        return Vec::new();
    }
    let count = |part: &[FileRow]| {
        let mut words = Words {
            wanted,
            seen: HashMap::new(),
        };
        part.iter()
            .map(|file| {
                let mut doc = Document {
                    counts: BTreeMap::new(),
                    length: 0,
                };
                words.note(&mut doc, &file.path);
                for symbol in &file.symbols {
                    words.note(&mut doc, &symbol.name);
                    words.note(&mut doc, &symbol.signature);
                }
                doc
            })
            .collect::<Vec<_>>()
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(THREADS);
    if !parallel_counting_worthwhile(threads, files.len()) {
        return count(files);
    }
    let size = files.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let parts: Vec<_> = files
            .chunks(size)
            .map(|part| scope.spawn(move || count(part)))
            .collect();
        parts
            .into_iter()
            .flat_map(|part| part.join().unwrap_or_default())
            .collect()
    })
}

/// Scheduling only: either branch counts and merges the same nonempty documents.
#[cfg_attr(test, mutants::skip)]
fn parallel_counting_worthwhile(threads: usize, files: usize) -> bool {
    threads >= 2 && files >= 2 * THREADS
}

/// The files ranked by BM25 for `wanted` (distinct terms), a test file at
/// [`TEST_SHARE`] of its score, at most `limit`; none unless the best file
/// holds [`MIN_TERMS`] of the terms, read before the test files are weighed.
/// Each lead points at the file's symbol whose own name and signature weigh
/// most, the file's first line when none holds a term.
pub(crate) fn rank(files: &[FileRow], wanted: &[String], limit: usize) -> Vec<MeaningHit> {
    let lookup: HashMap<&str, usize> = wanted
        .iter()
        .enumerate()
        .map(|(at, term)| (term.as_str(), at))
        .collect();
    let docs = documents(files, &lookup);
    let held: Vec<&Document> = docs.iter().filter(|doc| doc.length > 0).collect();
    if held.is_empty() {
        return Vec::new();
    }
    let n = held.len() as f64;
    let mean = (held.iter().map(|doc| doc.length).sum::<usize>() as f64 / n).max(1.0);
    let mut df = vec![0_usize; wanted.len()];
    for doc in &held {
        for &at in doc.counts.keys() {
            df[at] += 1;
        }
    }
    let idf: Vec<f64> = df
        .iter()
        .map(|&df| {
            let df = df as f64;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    let mut ranked: Vec<(usize, f64, usize)> = docs
        .iter()
        .enumerate()
        .filter(|(_, doc)| !doc.counts.is_empty())
        .map(|(file, doc)| {
            let norm = K1 * (1.0 - B + B * doc.length as f64 / mean);
            let score = doc
                .counts
                .iter()
                .map(|(&at, &count)| {
                    let tf = f64::from(count);
                    idf[at] * tf * (K1 + 1.0) / (tf + norm)
                })
                .sum();
            (file, score, doc.counts.len())
        })
        .collect();
    let by_score = |a: &(usize, f64, usize), b: &(usize, f64, usize)| {
        b.1.total_cmp(&a.1)
            .then_with(|| files[a.0].path.cmp(&files[b.0].path))
    };
    ranked.sort_by(by_score);
    if ranked.first().is_none_or(|best| best.2 < MIN_TERMS) {
        return Vec::new();
    }
    for entry in &mut ranked {
        if is_test(&files[entry.0].path) {
            entry.1 *= TEST_SHARE;
        }
    }
    ranked.sort_by(by_score);
    ranked
        .into_iter()
        .take(limit)
        .map(|(file, score, _)| lead(&files[file], &lookup, &idf, score))
        .collect()
}

/// The lead of `file`: its symbol whose own terms weigh most, first by line on
/// a tie.
fn lead(file: &FileRow, lookup: &HashMap<&str, usize>, idf: &[f64], score: f64) -> MeaningHit {
    let mut best: Option<(f64, &SymbolRow)> = None;
    for symbol in &file.symbols {
        let mut own: Vec<usize> = terms(&symbol.name)
            .into_iter()
            .chain(terms(&symbol.signature))
            .filter_map(|term| lookup.get(term.as_str()).copied())
            .collect();
        own.sort_unstable();
        own.dedup();
        let weight: f64 = own.iter().map(|&at| idf[at]).sum();
        if weight > 0.0 && best.is_none_or(|(held, _)| weight > held) {
            best = Some((weight, symbol));
        }
    }
    MeaningHit {
        path: file.path.clone(),
        start_line: best.map_or(1, |(_, symbol)| symbol.start_line),
        end_line: best.map_or(1, |(_, symbol)| symbol.end_line),
        symbol: best.map(|(_, symbol)| symbol.name.clone()),
        score,
        snippet: String::new(),
    }
}

/// A test file: a path with a `test`, `tests`, `spec` or `specs` word
/// (pixel-mvp's rule).
fn is_test(path: &str) -> bool {
    sub_words(path)
        .iter()
        .any(|word| matches!(word.as_str(), "test" | "tests" | "spec" | "specs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, signature: &str, line: u32) -> SymbolRow {
        SymbolRow {
            name: name.into(),
            signature: signature.into(),
            start_line: line,
            end_line: line + 5,
        }
    }

    fn file(path: &str, symbols: Vec<SymbolRow>) -> FileRow {
        FileRow {
            path: path.into(),
            symbols,
        }
    }

    fn paths(leads: &[MeaningHit]) -> Vec<&str> {
        leads.iter().map(|lead| lead.path.as_str()).collect()
    }

    /// A repository where `src/rename.rs` is about renaming a symbol and the
    /// other files each hold one of the prompt's words at most.
    fn corpus() -> Vec<FileRow> {
        vec![
            file(
                "src/rename.rs",
                vec![
                    symbol("Rename", "pub struct Rename {", 3),
                    symbol("rename_symbol", "pub fn rename_symbol(uid: &str) {", 20),
                ],
            ),
            file(
                "src/symbol.rs",
                vec![symbol("Symbol", "pub struct Symbol {", 1)],
            ),
            file(
                "src/daemon.rs",
                vec![symbol("serve", "pub fn serve() {", 8)],
            ),
            file("README.md", Vec::new()),
        ]
    }

    #[test]
    fn the_switch_should_be_on_unless_it_is_turned_off() {
        // `yes` and an unset variable were off while the switch was opt-in:
        // with the default on, only the three opt-outs leave the leads out.
        for value in [None, Some(""), Some("auto"), Some("1"), Some("yes")] {
            assert!(enabled(value), "{value:?}");
        }
        for value in [Some("0"), Some("false"), Some("off")] {
            assert!(!enabled(value), "{value:?}");
        }
    }

    #[test]
    fn an_inflected_prompt_should_reach_the_file_whose_names_hold_its_words() {
        // "renaming" and "symbols" meet `Rename` and `rename_symbol` through
        // pixel-mvp's stemmer; the file holding both words comes first, at
        // the symbol that holds both.
        let leads = rank(&corpus(), &prompt_terms("renaming symbols"), 8);
        assert_eq!(paths(&leads), ["src/rename.rs", "src/symbol.rs"]);
        assert_eq!(
            (leads[0].start_line, leads[0].symbol.as_deref()),
            (20, Some("rename_symbol"))
        );
        assert!(leads[0].score > leads[1].score);
        assert!(leads.iter().all(|lead| lead.snippet.is_empty()));
    }

    #[test]
    fn a_prompt_no_file_holds_two_words_of_should_get_no_leads() {
        // The repository names "symbol" and "daemon", never in one file: a
        // ranked list would still put a file first, so the leads stay empty.
        assert!(rank(&corpus(), &prompt_terms("weather symbol daemon"), 8).is_empty());
        // One word alone never qualifies, however rare.
        assert!(rank(&corpus(), &prompt_terms("rename"), 8).is_empty());
    }

    #[test]
    fn a_test_file_should_rank_below_the_code_it_tests() {
        // The test repeats the code's names in a shorter document, so BM25
        // alone ranks it first; at half its score it follows the code.
        let mut files = corpus();
        files[0].symbols.extend([
            symbol("Options", "pub struct Options {", 40),
            symbol(
                "parse_options",
                "pub fn parse_options(args: &[String]) -> Options {",
                50,
            ),
            symbol("report", "fn report(out: &mut String, uid: &str) {", 60),
        ]);
        files.push(file(
            "tests/rename_test.rs",
            vec![symbol(
                "rename_symbol_works",
                "fn rename_symbol_works() {",
                4,
            )],
        ));
        let leads = rank(&files, &prompt_terms("renaming symbols"), 8);
        assert_eq!(
            paths(&leads)[..2],
            ["src/rename.rs", "tests/rename_test.rs"]
        );
        // Its own score was above the code's: the halving is what moved it.
        assert!(leads[1].score / TEST_SHARE > leads[0].score, "{leads:?}");
    }

    #[test]
    fn the_limit_should_cut_the_ranked_files() {
        let leads = rank(&corpus(), &prompt_terms("renaming symbols"), 1);
        assert_eq!(paths(&leads), ["src/rename.rs"]);
    }

    #[test]
    fn a_file_ranked_for_its_path_should_point_at_its_first_line() {
        let files = vec![file("src/rename/symbol.rs", Vec::new())];
        let leads = rank(&files, &prompt_terms("rename symbol"), 8);
        assert_eq!((leads[0].start_line, leads[0].symbol.as_deref()), (1, None));
    }

    #[test]
    fn the_documents_should_count_what_pixel_mvps_terms_count_on_any_number_of_threads() {
        // The per-word cache and the threads are a speed-up of `terms`, never
        // a second tokeniser: each document's length is the count of `terms`
        // over its path, names and signatures.
        let files: Vec<FileRow> = (0..40)
            .map(|at| {
                file(
                    &format!("src/area_{at}/HTTPServer.rs"),
                    vec![symbol(
                        &format!("handle_{at}_requests"),
                        "pub fn handle(the request: &Request) -> Result<(), Error> {",
                        at,
                    )],
                )
            })
            .collect();
        let wanted = prompt_terms("handle requests");
        let lookup: HashMap<&str, usize> = wanted
            .iter()
            .enumerate()
            .map(|(at, term)| (term.as_str(), at))
            .collect();
        let docs = documents(&files, &lookup);
        assert_eq!(docs.len(), files.len());
        for (doc, file) in docs.iter().zip(&files) {
            let mut all = terms(&file.path);
            for symbol in &file.symbols {
                all.extend(terms(&symbol.name));
                all.extend(terms(&symbol.signature));
            }
            assert_eq!(doc.length, all.len(), "{}", file.path);
            for (at, term) in wanted.iter().enumerate() {
                let count = all.iter().filter(|held| *held == term).count();
                assert_eq!(
                    doc.counts.get(&at).copied().unwrap_or(0) as usize,
                    count,
                    "{} {term}",
                    file.path
                );
            }
        }
    }

    #[test]
    fn audited_bm25_should_preserve_exact_scores_document_frequency_and_length_normalization() {
        let files = vec![
            file("alpha_beta", vec![]),
            file("alpha", vec![]),
            file("gamma", vec![]),
            file("", vec![]),
        ];
        let hits = rank(&files, &["alpha".into(), "beta".into()], 8);
        assert_eq!(paths(&hits), ["alpha_beta", "alpha"]);
        assert!(
            (hits[0].score - 1.204_465_034_326_949_6).abs() < 1e-12,
            "{}",
            hits[0].score
        );
        assert!(
            (hits[1].score - 0.523_548_346_501_579).abs() < 1e-12,
            "{}",
            hits[1].score
        );
        assert_eq!(rank(&files, &["alpha".into(), "beta".into()], 1).len(), 1);
        assert_eq!(rank(&files, &["alpha".into(), "beta".into()], 0).len(), 0);
    }

    #[test]
    fn audited_document_chunks_should_keep_input_order_and_counts_across_the_parallel_threshold() {
        for count in [0, 7, 8, 9] {
            let files = (0..count)
                .map(|_| file("alpha_beta", vec![]))
                .collect::<Vec<_>>();
            let docs = documents(&files, &HashMap::from([("alpha", 0), ("beta", 1)]));
            assert_eq!(docs.len(), count);
            for doc in docs {
                assert_eq!(doc.length, 2);
                assert_eq!(doc.counts, BTreeMap::from([(0, 1), (1, 1)]));
            }
        }
    }

    #[test]
    fn audited_symbol_leads_should_ignore_zero_weights_and_keep_the_first_symbol_on_a_tie() {
        let files = vec![file(
            "alpha_beta",
            vec![
                symbol("unused", "", 1),
                symbol("alpha_beta", "", 10),
                symbol("alpha_beta", "", 20),
            ],
        )];
        let hits = rank(&files, &["alpha".into(), "beta".into()], 8);
        assert_eq!(
            (
                hits[0].start_line,
                hits[0].end_line,
                hits[0].symbol.as_deref()
            ),
            (10, 15, Some("alpha_beta"))
        );
        let without = rank(
            &[file("alpha_beta", vec![symbol("unused", "", 1)])],
            &["alpha".into(), "beta".into()],
            8,
        );
        assert_eq!(
            (without[0].start_line, without[0].symbol.as_deref()),
            (1, None)
        );
    }
}
