// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Runs the rule files over the files a plan names, on every core, within
//! an optional deadline, and turns each extraction into rows: uids, owners,
//! the `File` symbol.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mvp_model::hash::Hash;
use mvp_model::{Edge, FileRow, FileRows, Lang, Symbol, SymbolKind, uid_path};

use crate::lang::{self, Extraction};
use crate::text;
use crate::tree::{self, TreeFile};

/// What `extract` did with the files it was given.
#[derive(Debug, Default)]
pub struct Extracted {
    /// Sorted by path.
    pub files: Vec<FileRows>,
    /// Files the deadline did not reach (stale), sorted.
    pub stale: Vec<String>,
}

/// Extracts `files` from the tree at `root`, stopping at `deadline`: a file
/// not started by then is stale. A file that vanished since the scan is
/// dropped.
pub fn extract(root: &Path, files: &[TreeFile], deadline: Option<Instant>) -> Extracted {
    let next = AtomicUsize::new(0);
    let done = Mutex::new(Vec::with_capacity(files.len()));
    let stale = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 16)
        .min(files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { break };
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        stale.lock().expect("stale list").push(file.path.clone());
                        continue;
                    }
                    let Ok(content) = std::fs::read(root.join(&file.path)) else {
                        continue;
                    };
                    let rows = rows(&file.path, file.lang, &content);
                    done.lock().expect("extracted list").push(rows);
                }
            });
        }
    });
    let mut files = done.into_inner().expect("extracted list");
    files.sort_by(|a, b| a.file.path.cmp(&b.file.path));
    let mut stale = stale.into_inner().expect("stale list");
    stale.sort();
    Extracted { files, stale }
}

/// What the stored facts of a file indexed unread start with, before what
/// [`Rules::unread`](lang::Rules::unread) recorded: `link` tells such a
/// file by it and leaves it out of every decision but through
/// `World::unread`. No file read has facts that start with it: rule files
/// write facts with `codec`, which puts a backslash only before itself and
/// its two separators.
pub const UNREAD: &str = "\\unread\n";

/// One file's rows from its content: the rule file's extraction, or its
/// reading of a file indexed unread ([`tree::unread`]) behind [`UNREAD`],
/// plus the file's own `File` symbol, uids, and each site's owner.
pub fn rows(path: &str, lang: Lang, content: &[u8]) -> FileRows {
    let rules = lang::rules(lang);
    let extraction = if tree::unread(lang, path, content) {
        let mut recorded = rules.unread(path, content);
        recorded.facts.insert_str(0, UNREAD);
        recorded
    } else {
        rules.extract(path, content)
    };
    finish(
        path,
        lang,
        Hash::of(content),
        text::line_count(content),
        extraction,
    )
}

fn finish(path: &str, lang: Lang, hash: Hash, lines: u32, extraction: Extraction) -> FileRows {
    let Extraction { defs, sites, facts } = extraction;
    let file_uid = uid(path, path, SymbolKind::File, 1);
    let mut symbols = Vec::with_capacity(defs.len() + 1);
    symbols.push(Symbol {
        uid: file_uid,
        file: path.to_owned(),
        kind: SymbolKind::File,
        name: path.to_owned(),
        signature: String::new(),
        line: 1,
        end_line: lines,
    });
    let mut seen: HashMap<(String, SymbolKind), u32> = HashMap::new();
    for def in &defs {
        let count = seen.entry((def.name.clone(), def.kind)).or_insert(0);
        *count += 1;
        symbols.push(Symbol {
            uid: uid(path, &def.name, def.kind, *count),
            file: path.to_owned(),
            kind: def.kind,
            name: def.name.clone(),
            signature: def.signature.clone(),
            line: def.line,
            end_line: def.end_line.max(def.line),
        });
    }
    let edges = sites
        .into_iter()
        .map(|site| {
            let owner = match site.owner {
                Some(lang::FILE_OWNER) => 0,
                Some(i) if i < defs.len() => i + 1,
                _ => smallest_spanning(&symbols, site.line),
            };
            Edge {
                src: symbols[owner].uid.clone(),
                dst: None,
                kind: site.kind,
                line: site.line,
                target: site.target,
                context: site.context,
            }
        })
        .collect();
    FileRows {
        file: FileRow {
            path: path.to_owned(),
            lang,
            hash,
        },
        facts,
        symbols,
        edges,
    }
}

/// `path#name#kind`, then `~n` from the second repetition on. The path is
/// written by [`uid_path`], so its end is the first `#` and the kind starts
/// after the last: a name may hold `#` (`User#name`), and two symbols never
/// share a uid however their paths are spelt.
pub fn uid(path: &str, name: &str, kind: SymbolKind, repetition: u32) -> String {
    let path = uid_path(path);
    if repetition <= 1 {
        format!("{path}#{name}#{kind}")
    } else {
        format!("{path}#{name}#{kind}~{repetition}")
    }
}

/// The index of the smallest symbol whose span holds `line`; the first one
/// on a tie, the `File` symbol (index 0) when nothing else does.
fn smallest_spanning(symbols: &[Symbol], line: u32) -> usize {
    let mut best = 0;
    let mut best_span = u32::MAX;
    for (i, symbol) in symbols.iter().enumerate().skip(1) {
        if symbol.line <= line && line <= symbol.end_line {
            let span = symbol.end_line - symbol.line;
            if span < best_span {
                best = i;
                best_span = span;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use mvp_model::EdgeKind;

    use super::*;
    use crate::lang::{Def, Site};

    fn def(kind: SymbolKind, name: &str, line: u32, end_line: u32) -> Def {
        Def {
            kind,
            name: name.to_owned(),
            signature: format!("{name} at {line}"),
            line,
            end_line,
        }
    }

    fn site(line: u32, owner: Option<usize>) -> Site {
        Site {
            kind: EdgeKind::Call,
            line,
            owner,
            target: "run".to_owned(),
            context: String::new(),
        }
    }

    #[test]
    fn a_repeated_definition_gets_a_numbered_uid() {
        let rows = finish(
            "a.rb",
            Lang::Ruby,
            Hash::of(b""),
            9,
            Extraction {
                defs: vec![
                    def(SymbolKind::Method, "A#run", 1, 2),
                    def(SymbolKind::Method, "A#run", 4, 5),
                    def(SymbolKind::Class, "A#run", 7, 8),
                ],
                ..Extraction::default()
            },
        );
        let uids: Vec<&str> = rows.symbols.iter().map(|s| s.uid.as_str()).collect();
        assert_eq!(
            uids,
            [
                "a.rb#a.rb#file",
                "a.rb#A#run#method",
                "a.rb#A#run#method~2",
                "a.rb#A#run#class"
            ]
        );
    }

    // D24: a file past the cap or a generated blob is read by
    // `Rules::unread`, whose default knows nothing of its definitions.
    #[test]
    fn a_file_indexed_unread_yields_its_file_symbol_only() {
        let large = format!(
            "def helper\nend\n{}",
            "helper\n".repeat(crate::tree::SIZE_CAP as usize / 7 + 1)
        );
        let blob = format!("def helper; end; {}\n", "helper; ".repeat(10_000));
        let rust_blob = "fn helper() {} ".repeat(5_000);
        for (path, lang, content) in [
            ("lib/big.rb", Lang::Ruby, large.as_str()),
            ("lib/blob.rb", Lang::Ruby, blob.as_str()),
            ("src/blob.rs", Lang::Rust, rust_blob.as_str()),
        ] {
            let rows = rows(path, lang, content.as_bytes());
            let kinds: Vec<SymbolKind> = rows.symbols.iter().map(|s| s.kind).collect();
            assert_eq!(kinds, [SymbolKind::File], "{path}");
            assert!(rows.edges.is_empty(), "{path}");
            assert_eq!(rows.facts, UNREAD, "{path}");
            assert_eq!(rows.file.hash, Hash::of(content.as_bytes()));
        }
    }

    // D24-R1: `link` tells a file indexed unread by the mark its facts
    // start with, so facts a rule file wrote must never start with it.
    #[test]
    fn facts_written_with_the_codec_never_start_with_the_unread_mark() {
        for field in [UNREAD, "\\", "\\unread"] {
            assert!(!crate::codec::record(&[field]).starts_with(UNREAD));
            assert!(!crate::codec::records(&[[field, "x"]]).starts_with(UNREAD));
        }
    }

    // D44
    #[test]
    fn a_hash_sign_or_a_percent_sign_in_a_path_keeps_uids_distinct() {
        assert_eq!(
            uid("bin/a", "X#y", SymbolKind::Method, 1),
            "bin/a#X#y#method"
        );
        assert_ne!(
            uid("bin/a", "X#y", SymbolKind::Method, 1),
            uid("bin/a#X", "y", SymbolKind::Method, 1)
        );
        assert_ne!(
            uid("bin/a#", "b", SymbolKind::File, 1),
            uid("bin/a%23", "b", SymbolKind::File, 1)
        );
        assert_eq!(
            uid("bin/a#X%", "y", SymbolKind::Method, 2),
            "bin/a%23X%25#y#method~2"
        );
    }

    #[test]
    fn a_site_without_owner_belongs_to_the_smallest_span_holding_it() {
        let rows = finish(
            "a.rb",
            Lang::Ruby,
            Hash::of(b""),
            20,
            Extraction {
                defs: vec![
                    def(SymbolKind::Class, "A", 1, 10),
                    def(SymbolKind::Method, "A#run", 2, 4),
                ],
                sites: vec![
                    site(3, None),
                    site(6, None),
                    site(15, None),
                    site(3, Some(0)),
                    site(3, Some(lang::FILE_OWNER)),
                ],
                ..Extraction::default()
            },
        );
        let owners: Vec<&str> = rows.edges.iter().map(|e| e.src.as_str()).collect();
        assert_eq!(
            owners,
            [
                "a.rb#A#run#method",
                "a.rb#A#class",
                "a.rb#a.rb#file",
                "a.rb#A#class",
                "a.rb#a.rb#file"
            ]
        );
    }
}
