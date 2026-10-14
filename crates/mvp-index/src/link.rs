// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! From a tree and the current generation to the next one: which files to
//! extract again (`plan`), then every site of every language decided again
//! against the whole generation (`link`). Deciding every site each time
//! makes an incremental run store exactly what a full run stores.

use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use mvp_model::hash::{Hash, Hasher};
use mvp_model::{Delta, Edge, FileRow, FileRows, Kept, Lang, Provenance, Symbol};

use crate::extract::{Extracted, UNREAD};
use crate::lang::{self, World};
use crate::tree::{Tree, TreeFile};

/// What the next generation needs from the files on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// New files, changed files, and every file of a language whose
    /// extractor version changed; sorted by path.
    pub extract: Vec<TreeFile>,
    /// Indexed paths whose rows go: removed from the tree, or replaced by a
    /// new extraction; sorted.
    pub drop: Vec<String>,
    /// The paths of `drop` whose content changed, same language and
    /// extractor version: their rows are what the rules make of the
    /// previous content, so a run that leaves one stale keeps them; sorted.
    pub changed: Vec<String>,
    /// The files the run that published the current generation left stale
    /// (`Delta::stale`). While there is one, that generation is not the
    /// full index of the tree, even once the tree has restored or removed
    /// them: a stale file it left out left its language unresolved, and the
    /// stale list is part of its id. The next run decides every site again.
    pub left_stale: Vec<String>,
}

impl Plan {
    /// Nothing to extract, nothing to drop, and no file left stale by the
    /// current generation: the index is the full index of the tree, fresh.
    pub fn is_empty(&self) -> bool {
        self.extract.is_empty() && self.drop.is_empty() && self.left_stale.is_empty()
    }

    /// The files of `left_stale` that the plan still extracts. A stale file
    /// the tree has since restored or removed is stale no more, so it is
    /// not counted (C-11); one still to read is in `extract`.
    pub fn stale(&self) -> Vec<&str> {
        self.left_stale
            .iter()
            .filter(|path| {
                self.extract
                    .binary_search_by(|file| file.path.as_str().cmp(path.as_str()))
                    .is_ok()
            })
            .map(String::as_str)
            .collect()
    }
}

/// Compares the tree with the current generation: its files, the extractor
/// version it was built with (`None` when there is none) and the files its
/// run left stale.
pub fn plan(
    tree: &Tree,
    indexed: &[FileRow],
    indexed_extractor: Option<&str>,
    indexed_stale: &[String],
) -> Plan {
    let current: HashMap<&str, &FileRow> = indexed.iter().map(|f| (f.path.as_str(), f)).collect();
    let outdated = |file_lang: Lang| {
        indexed_extractor.is_none_or(|stored| {
            version_of(stored, file_lang) != Some(lang::rules(file_lang).version())
        })
    };
    let mut extract = Vec::new();
    let mut drop = BTreeSet::new();
    let mut changed = Vec::new();
    for file in &tree.files {
        match current.get(file.path.as_str()) {
            Some(row) if row.lang == file.lang && !outdated(file.lang) => {
                if row.hash != file.hash {
                    drop.insert(file.path.clone());
                    changed.push(file.path.clone());
                    extract.push(file.clone());
                }
            }
            Some(_) => {
                drop.insert(file.path.clone());
                extract.push(file.clone());
            }
            None => extract.push(file.clone()),
        }
    }
    let listed: BTreeSet<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
    for row in indexed {
        if !listed.contains(row.path.as_str()) {
            drop.insert(row.path.clone());
        }
    }
    Plan {
        extract,
        drop: drop.into_iter().collect(),
        changed,
        left_stale: indexed_stale.to_vec(),
    }
}

/// The version a stored extractor string (`ruby.1,rust.1`) gives `lang`.
fn version_of(extractor: &str, lang: Lang) -> Option<u32> {
    extractor.split(',').find_map(|part| {
        let (name, version) = part.split_once('.')?;
        (name == lang.as_str())
            .then(|| version.parse().ok())
            .flatten()
    })
}

/// Decides every site of the next generation and describes it as a delta.
/// A file of `plan.changed` the deadline left stale keeps its previous
/// rows and hash, so the generation is the full index of the tree with that
/// file as it was, and the next run reads it. Any other stale file is
/// absent, being new or having its previous rows go, and no site of its
/// language is resolved in this generation ([`undecidable`]); the run that
/// reads it decides them all again (C-10), and so does a run that finds
/// it gone (`Plan::left_stale`). A language with a file indexed unread
/// (`extract::UNREAD`) has none of its sites resolved: what the file
/// defines is unknown, and no rule file reads `World::unread` yet (D24,
/// `open-questions.md` Q37). `kept` loads the rows of
/// the current generation outside the paths that go; it is not called when
/// the plan is empty, so a fresh index costs no full read.
pub fn link<E>(
    tree: &Tree,
    plan: &Plan,
    extracted: Extracted,
    kept: impl FnOnce(&[String]) -> Result<Kept, E>,
) -> Result<Delta, E> {
    let Extracted {
        files: mut add,
        stale,
    } = extracted;
    if plan.is_empty() && add.is_empty() && stale.is_empty() {
        let files: Vec<FileRow> = tree
            .files
            .iter()
            .map(|f| FileRow {
                path: f.path.clone(),
                lang: f.lang,
                hash: f.hash,
            })
            .collect();
        return Ok(delta(
            tree,
            tree.dirty,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            files,
        ));
    }
    let late: BTreeSet<&str> = stale.iter().map(String::as_str).collect();
    let drop: Vec<String> = plan
        .drop
        .iter()
        .filter(|path| !(late.contains(path.as_str()) && plan.changed.binary_search(path).is_ok()))
        .cloned()
        .collect();
    let blind = undecidable(plan, &late);
    let Kept {
        files: mut kept_files,
        symbols: kept_symbols,
        edges: kept_edges,
    } = kept(&drop)?;
    let file_of_uid: HashMap<&str, &str> = kept_symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.file.as_str()))
        .collect();
    let mut redirect = Vec::new();
    for rules in lang::RULES {
        let lang = rules.lang();
        let mut files: Vec<(String, String)> = Vec::new();
        let mut unread: Vec<(String, String)> = Vec::new();
        let mut symbols: Vec<Symbol> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();
        let mut kept_lang = BTreeSet::new();
        // Each file's facts are moved, not copied, into what the rule file
        // reads: a workspace's may be large. The kept rows need theirs no
        // more; the fresh ones get theirs back below.
        for (file, facts) in kept_files.iter_mut().filter(|(f, _)| f.lang == lang) {
            let file: &FileRow = file;
            if let Some(recorded) = facts.strip_prefix(UNREAD) {
                unread.push((file.path.clone(), recorded.to_owned()));
                continue;
            }
            kept_lang.insert(file.path.as_str());
            files.push((file.path.clone(), std::mem::take(facts)));
        }
        symbols.extend(
            kept_symbols
                .iter()
                .filter(|s| kept_lang.contains(s.file.as_str()))
                .cloned(),
        );
        let kept_edge_indices: Vec<usize> = kept_edges
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                file_of_uid
                    .get(e.src.as_str())
                    .is_some_and(|file| kept_lang.contains(file))
            })
            .map(|(i, _)| i)
            .collect();
        edges.extend(kept_edge_indices.iter().map(|i| kept_edges[*i].clone()));
        let mut fresh: Vec<usize> = Vec::new();
        for (i, rows) in add.iter_mut().enumerate() {
            if rows.file.lang != lang {
                continue;
            }
            if let Some(recorded) = rows.facts.strip_prefix(UNREAD) {
                unread.push((rows.file.path.clone(), recorded.to_owned()));
                continue;
            }
            fresh.push(i);
            files.push((rows.file.path.clone(), std::mem::take(&mut rows.facts)));
            symbols.extend(rows.symbols.iter().cloned());
            edges.extend(rows.edges.iter().cloned());
        }
        let decided = if edges.is_empty() || blind.contains(&lang) || !unread.is_empty() {
            Vec::new()
        } else {
            rules.resolve(&World {
                files: &files,
                symbols: &symbols,
                edges: &edges,
                unread: &unread,
            })
        };
        let first = files.len() - fresh.len();
        for (moved, i) in files.drain(first..).zip(&fresh) {
            add[*i].facts = moved.1;
        }
        if edges.is_empty() {
            continue;
        }
        let dst = |position: usize| -> Option<String> {
            decided
                .get(position)
                .copied()
                .flatten()
                .and_then(|s| symbols.get(s))
                .map(|s| s.uid.clone())
        };
        for (position, original) in kept_edge_indices.iter().enumerate() {
            let new = dst(position);
            if kept_edges[*original].dst != new {
                redirect.push((kept_edges[*original].clone(), new));
            }
        }
        let mut position = kept_edge_indices.len();
        for i in fresh {
            for edge in &mut add[i].edges {
                edge.dst = dst(position);
                position += 1;
            }
        }
    }
    let dirty = tree.dirty || drifted(plan, &add, &stale);
    let mut files: Vec<FileRow> = kept_files.into_iter().map(|(file, _)| file).collect();
    files.extend(add.iter().map(|rows| rows.file.clone()));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(delta(tree, dirty, drop, add, redirect, stale, files))
}

/// The languages whose sites this generation cannot decide: that of each
/// file the deadline left stale that the generation leaves out, a new file
/// or one whose previous rows go (another extractor version's or another
/// language's reading of it). What it defines is unknown, so a name it may
/// also define would resolve to another file's: every site of its language
/// stays unresolved, in each run that leaves it stale, until the run that
/// reads it or finds it gone (`Plan::left_stale`) (C-10). A file of
/// `plan.changed` left stale keeps its rows, so what it defines is known,
/// as of its previous content.
fn undecidable(plan: &Plan, late: &BTreeSet<&str>) -> BTreeSet<Lang> {
    plan.extract
        .iter()
        .filter(|file| {
            late.contains(file.path.as_str()) && plan.changed.binary_search(&file.path).is_err()
        })
        .map(|file| file.lang)
        .collect()
}

/// Whether a planned file changed or vanished between the scan and its
/// extraction, which reads the disk again: the rows then match neither the
/// scan's commit nor its clean flag (C-11). A stale file was not read.
fn drifted(plan: &Plan, add: &[FileRows], stale: &[String]) -> bool {
    let read: HashMap<&str, Hash> = add
        .iter()
        .map(|rows| (rows.file.path.as_str(), rows.file.hash))
        .collect();
    plan.extract.iter().any(|file| {
        stale.binary_search(&file.path).is_err() && read.get(file.path.as_str()) != Some(&file.hash)
    })
}

fn delta(
    tree: &Tree,
    dirty: bool,
    drop: Vec<String>,
    add: Vec<FileRows>,
    redirect: Vec<(Edge, Option<String>)>,
    stale: Vec<String>,
    files: Vec<FileRow>,
) -> Delta {
    let extractor = lang::extractor();
    Delta {
        provenance: Provenance {
            commit: tree.commit.clone(),
            dirty,
            generation: generation_id(&files, &extractor, &stale),
            extractor,
            indexed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        },
        drop,
        add,
        redirect,
        stale,
        files,
        stats: tree
            .files
            .iter()
            .map(|file| (file.path.clone(), file.stat))
            .collect(),
    }
}

/// SHA-256 of the extractor version, the sorted (path, content hash) list
/// and, when the deadline left files stale, their sorted paths
/// (`design/architecture.md`, "Index generations"). The stale paths decide
/// rows too: a stale file the generation leaves out leaves its language
/// unresolved ([`undecidable`]), so the same files with and without it
/// stale are two generations, with two ids (C-11).
pub fn generation_id(files: &[FileRow], extractor: &str, stale: &[String]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(extractor.as_bytes());
    hasher.update(b"\n");
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&file.hash.0);
        hasher.update(b"\n");
    }
    if !stale.is_empty() {
        // No path is empty or holds a NUL, so no file's entry starts here
        // and each stale path ends at the next NUL.
        hasher.update(b"\0stale");
        for path in stale {
            hasher.update(b"\0");
            hasher.update(path.as_bytes());
        }
    }
    hasher.finish().to_hex()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use mvp_model::hash::Hash;

    use super::*;

    fn tree(files: &[(&str, Lang, &str)]) -> Tree {
        Tree {
            root: PathBuf::from("/repo"),
            commit: None,
            dirty: false,
            files: files
                .iter()
                .map(|(path, lang, content)| TreeFile {
                    path: (*path).to_owned(),
                    lang: *lang,
                    hash: Hash::of(content.as_bytes()),
                    bytes: content.len() as u64,
                    stat: None,
                })
                .collect(),
            skipped: Vec::new(),
            untracked: Vec::new(),
        }
    }

    fn indexed(files: &[(&str, Lang, &str)]) -> Vec<FileRow> {
        files
            .iter()
            .map(|(path, lang, content)| FileRow {
                path: (*path).to_owned(),
                lang: *lang,
                hash: Hash::of(content.as_bytes()),
            })
            .collect()
    }

    fn extracted(plan: &Plan) -> Vec<&str> {
        plan.extract.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn only_new_and_changed_files_are_extracted_and_removed_ones_dropped() {
        let current = lang::extractor();
        let stored = indexed(&[
            ("a.rb", Lang::Ruby, "def a; end"),
            ("b.rb", Lang::Ruby, "def b; end"),
            ("gone.rs", Lang::Rust, "fn gone() {}"),
        ]);
        let now = tree(&[
            ("a.rb", Lang::Ruby, "def a; end"),
            ("b.rb", Lang::Ruby, "def b; c; end"),
            ("new.rs", Lang::Rust, "fn new() {}"),
        ]);
        let plan = plan(&now, &stored, Some(&current), &[]);
        assert_eq!(extracted(&plan), ["b.rb", "new.rs"]);
        assert_eq!(plan.drop, ["b.rb", "gone.rs"]);
        assert_eq!(plan.changed, ["b.rb"]);
    }

    // D53
    #[test]
    fn c11_a_stale_file_counts_while_the_tree_still_holds_it() {
        let files = [("a.rb", Lang::Ruby, "def a; end")];
        let now = tree(&[
            ("a.rb", Lang::Ruby, "def a; end"),
            ("b\nc.rb", Lang::Ruby, "def b; end"),
        ]);
        let stale = ["b\nc.rb".to_owned(), "gone.rb".to_owned()];
        let extractor = lang::extractor();
        let pending = plan(&now, &indexed(&files), Some(&extractor), &stale);
        assert_eq!(pending.stale(), ["b\nc.rb"]);
        // IDX-FINAL2-1: gone, they are counted no more, but the generation
        // that left them stale is not the full index of the tree.
        let done = plan(&tree(&files), &indexed(&files), Some(&extractor), &stale);
        assert!(done.extract.is_empty() && done.drop.is_empty());
        assert!(done.stale().is_empty());
        assert!(!done.is_empty());
        assert!(plan(&tree(&files), &indexed(&files), Some(&extractor), &[]).is_empty());
    }

    #[test]
    fn an_unchanged_tree_needs_nothing() {
        let files = [
            ("a.rb", Lang::Ruby, "def a; end"),
            ("b.rs", Lang::Rust, "fn b() {}"),
        ];
        let plan = plan(
            &tree(&files),
            &indexed(&files),
            Some(&lang::extractor()),
            &[],
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn c19_an_extractor_version_bump_reextracts_every_file_of_its_language() {
        let files = [
            ("a.rb", Lang::Ruby, "def a; end"),
            ("b.rb", Lang::Ruby, "def b; end"),
            ("c.rs", Lang::Rust, "fn c() {}"),
        ];
        let rust = lang::rules(Lang::Rust).version();
        let other_ruby = format!("ruby.{},rust.{rust}", lang::rules(Lang::Ruby).version() + 1);
        let plan = plan(&tree(&files), &indexed(&files), Some(&other_ruby), &[]);
        assert_eq!(extracted(&plan), ["a.rb", "b.rb"]);
        assert_eq!(plan.drop, ["a.rb", "b.rb"]);
        assert!(plan.changed.is_empty());
    }

    #[test]
    fn c19_an_index_that_names_no_extractor_is_read_again_whole() {
        let files = [
            ("a.rb", Lang::Ruby, "def a; end"),
            ("c.rs", Lang::Rust, "fn c() {}"),
        ];
        for stored in [None, Some(""), Some("garbage")] {
            let plan = plan(&tree(&files), &indexed(&files), stored, &[]);
            assert_eq!(extracted(&plan), ["a.rb", "c.rs"], "{stored:?}");
        }
    }

    #[test]
    fn c11_a_file_changed_between_the_scan_and_its_extraction_marks_the_tree_dirty() {
        // D30: extraction reads the disk again, after the scan named a clean
        // commit; rows read from other content cannot carry it as clean.
        let scanned = tree(&[("a.rb", Lang::Ruby, "def a; end")]);
        let plan = plan(&scanned, &[], None, &[]);
        let link_read = |files: Vec<FileRows>, stale: Vec<String>| {
            link(&scanned, &plan, Extracted { files, stale }, |_| {
                Ok::<_, ()>(Kept::default())
            })
            .unwrap()
            .provenance
            .dirty
        };
        let rows = |content: &str| crate::extract::rows("a.rb", Lang::Ruby, content.as_bytes());
        assert!(!link_read(vec![rows("def a; end")], Vec::new()));
        assert!(!link_read(Vec::new(), vec!["a.rb".to_owned()]));
        assert!(link_read(vec![rows("def edited; end")], Vec::new()));
        assert!(link_read(Vec::new(), Vec::new()), "a vanished file");
    }
}

#[cfg(test)]
mod budget {
    //! A run whose deadline leaves files stale, against an in-memory store:
    //! `kept` and `publish` do what `mvp-store`'s writer does.

    use std::path::Path;

    use mvp_model::{Delta, FileRows, Kept};

    use super::*;
    use crate::extract::extract;
    use crate::truth::{index, write_tree};

    fn owned(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|(path, content)| ((*path).to_owned(), (*content).to_owned()))
            .collect()
    }

    /// The rows of `previous` outside `drop`.
    fn kept(previous: &[FileRows], drop: &[String]) -> Kept {
        let mut kept = Kept::default();
        for rows in previous.iter().filter(|r| !drop.contains(&r.file.path)) {
            kept.files.push((rows.file.clone(), rows.facts.clone()));
            kept.symbols.extend(rows.symbols.iter().cloned());
            kept.edges.extend(rows.edges.iter().cloned());
        }
        kept
    }

    /// `previous` once `delta` is published over it.
    fn publish(previous: &[FileRows], delta: &Delta) -> Vec<FileRows> {
        let mut rows: Vec<FileRows> = previous
            .iter()
            .filter(|r| !delta.drop.contains(&r.file.path))
            .cloned()
            .collect();
        for (edge, dst) in &delta.redirect {
            for stored in rows.iter_mut().flat_map(|r| &mut r.edges) {
                if *stored == *edge {
                    stored.dst = dst.clone();
                }
            }
        }
        rows.extend(delta.add.iter().cloned());
        rows.sort_by(|a, b| a.file.path.cmp(&b.file.path));
        rows
    }

    /// The full index of `before`, then a run over `after` whose deadline
    /// never reaches the files of `stale`: what the store holds then, and
    /// the delta.
    fn budget_run(
        root: &Path,
        before: &[(String, String)],
        after: &[(String, String)],
        stale: &[&str],
    ) -> (Vec<FileRows>, Delta) {
        let previous = index(root, before);
        for (path, _) in before {
            std::fs::remove_file(root.join(path)).expect("removed");
        }
        let tree = write_tree(root, after);
        let indexed: Vec<FileRow> = previous.iter().map(|r| r.file.clone()).collect();
        let plan = plan(&tree, &indexed, Some(&lang::extractor()), &[]);
        run(&tree, &plan, &previous, stale)
    }

    /// A run of `plan` over `tree` and the store holding `previous`, whose
    /// deadline never reaches the files of `stale`: what the store holds
    /// then, and the delta.
    fn run(
        tree: &Tree,
        plan: &Plan,
        previous: &[FileRows],
        stale: &[&str],
    ) -> (Vec<FileRows>, Delta) {
        let (late, read): (Vec<TreeFile>, Vec<TreeFile>) = plan
            .extract
            .iter()
            .cloned()
            .partition(|f| stale.contains(&f.path.as_str()));
        let mut extracted = extract(&tree.root, &read, None);
        extracted.stale = late.into_iter().map(|f| f.path).collect();
        let delta = link(tree, plan, extracted, |drop| {
            Ok::<_, ()>(kept(previous, drop))
        })
        .expect("no store to fail");
        (publish(previous, &delta), delta)
    }

    // D02: a changed file left stale was dropped, and the sites of the files
    // read in time were decided without it.
    #[test]
    fn c10_a_budget_run_stores_the_full_index_of_the_tree_with_its_stale_files_unchanged() {
        let cases = [
            // A name two files define stays ambiguous, and a third file
            // read in time joins them.
            (
                vec![
                    ("lib/a.rb", "def helper\n  :a\nend\n"),
                    ("lib/b.rb", "def helper\n  :b\nend\n"),
                    (
                        "lib/c.rb",
                        "require_relative \"a\"\nrequire_relative \"b\"\n\nclass C\n  def run\n    helper\n  end\nend\n",
                    ),
                ],
                vec![
                    ("lib/a.rb", "def helper\n  :a\nend\n"),
                    ("lib/b.rb", "def helper\n  :b\nend\n# changed\n"),
                    (
                        "lib/c.rb",
                        "require_relative \"a\"\nrequire_relative \"b\"\n\nclass C\n  def run\n    helper\n  end\n\n  def go\n    helper\n  end\nend\n",
                    ),
                    ("lib/d.rb", "def helper\n  :d\nend\n"),
                ],
                vec!["lib/b.rb"],
            ),
            // An override in a stale file still wins over the superclass.
            (
                vec![
                    (
                        "app/models/user.rb",
                        "class User\n  def name\n    :user\n  end\nend\n",
                    ),
                    (
                        "app/models/admin.rb",
                        "class Admin < User\n  def run\n    name\n  end\nend\n",
                    ),
                    (
                        "app/models/admin_name.rb",
                        "class Admin < User\n  def name\n    :admin\n  end\nend\n",
                    ),
                ],
                vec![
                    (
                        "app/models/user.rb",
                        "class User\n  def name\n    :user\n  end\nend\n",
                    ),
                    (
                        "app/models/admin.rb",
                        "class Admin < User\n  def run\n    name\n  end\n\n  def go\n    name\n  end\nend\n",
                    ),
                    (
                        "app/models/admin_name.rb",
                        "class Admin < User\n  def name\n    :admin\n  end\nend\n# changed\n",
                    ),
                ],
                vec!["app/models/admin_name.rb"],
            ),
            // An inherent method in a stale file still wins over the trait's.
            (
                vec![
                    ("Cargo.toml", "[package]\nname = \"brs\"\n"),
                    (
                        "src/lib.rs",
                        "pub mod a;\npub mod b;\npub mod c;\n\npub struct S;\n\npub trait Named {\n    fn name(&self) -> u32;\n}\n",
                    ),
                    (
                        "src/a.rs",
                        "use crate::S;\n\nimpl S {\n    pub fn name(&self) -> u32 {\n        1\n    }\n}\n",
                    ),
                    (
                        "src/b.rs",
                        "use crate::{Named, S};\n\nimpl Named for S {\n    fn name(&self) -> u32 {\n        2\n    }\n}\n",
                    ),
                    (
                        "src/c.rs",
                        "use crate::{Named, S};\n\npub fn run(s: S) -> u32 {\n    s.name()\n}\n",
                    ),
                ],
                vec![
                    ("Cargo.toml", "[package]\nname = \"brs\"\n"),
                    (
                        "src/lib.rs",
                        "pub mod a;\npub mod b;\npub mod c;\n\npub struct S;\n\npub trait Named {\n    fn name(&self) -> u32;\n}\n",
                    ),
                    (
                        "src/a.rs",
                        "use crate::S;\n\nimpl S {\n    pub fn name(&self) -> u32 {\n        1\n    }\n}\n// changed\n",
                    ),
                    (
                        "src/b.rs",
                        "use crate::{Named, S};\n\nimpl Named for S {\n    fn name(&self) -> u32 {\n        2\n    }\n}\n",
                    ),
                    (
                        "src/c.rs",
                        "use crate::{Named, S};\n\npub fn run(s: S) -> u32 {\n    s.name()\n}\n\npub fn go(s: S) -> u32 {\n    s.name()\n}\n",
                    ),
                ],
                vec!["src/a.rs"],
            ),
        ];
        for (before, after, stale) in cases {
            stores_the_tree_with_its_stale_files_unchanged(&owned(&before), &owned(&after), &stale);
        }
    }

    /// Runs `budget_run` and checks that the store then holds the full
    /// index of `after` with each file of `stale`, changed since `before`,
    /// at its content there; returns what it holds. A new file left stale
    /// is another case: `c10_a_new_file_left_stale_leaves_every_site_of_its_language_unresolved`.
    fn stores_the_tree_with_its_stale_files_unchanged(
        before: &[(String, String)],
        after: &[(String, String)],
        stale: &[&str],
    ) -> Vec<FileRows> {
        let dir = tempfile::tempdir().unwrap();
        let (stored, delta) = budget_run(dir.path(), before, after, stale);
        assert_eq!(delta.stale, stale, "every file but these was read");
        let mixed: Vec<(String, String)> = after
            .iter()
            .map(|(path, content)| {
                if !stale.contains(&path.as_str()) {
                    return (path.clone(), content.clone());
                }
                before
                    .iter()
                    .find(|(p, _)| p == path)
                    .cloned()
                    .expect("a stale file `before` holds")
            })
            .collect();
        let full = index(tempfile::tempdir().unwrap().path(), &mixed);
        let files: Vec<FileRow> = full.iter().map(|r| r.file.clone()).collect();
        assert_eq!(delta.files, files, "{stale:?}");
        assert_eq!(stored, full, "{stale:?}");
        stored
    }

    /// A workspace whose `app` glob-imports `big`, a module that defines
    /// `util::run` in the first lines, then `padding` comment lines.
    fn shadowing(padding: usize) -> Vec<(String, String)> {
        let big = format!(
            "pub mod util {{\n    pub fn run() -> u32 {{\n        2\n    }}\n}}\n{}",
            "// padding\n".repeat(padding)
        );
        let main = "mod big;\nuse big::*;\n\nfn main() {\n    let x = util::run();\n    println!(\"{}\", x);\n}\n";
        owned(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"util\", \"app\"]\n"),
            ("util/Cargo.toml", "[package]\nname = \"util\"\n"),
            ("util/src/lib.rs", "pub fn run() -> u32 {\n    1\n}\n"),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nutil = { path = \"../util\" }\n",
            ),
            ("app/src/main.rs", main),
            ("app/src/big.rs", &big),
        ])
    }

    // D24-R1: a file indexed unread was decided on as a file that defines
    // nothing, whether read in the run or kept by it, so a call it may
    // shadow resolved past it.
    #[test]
    fn c10_a_file_indexed_unread_is_unknown_to_every_decision_read_or_kept() {
        let past_the_cap = 110_000;
        for (before, after) in [(0, past_the_cap), (past_the_cap, 0)] {
            let stored = stores_the_tree_with_its_stale_files_unchanged(
                &shadowing(before),
                &shadowing(after),
                &["app/src/big.rs"],
            );
            let run = targets(&stored, "util::run");
            if before == past_the_cap {
                assert_eq!(run, [None], "kept unread");
            } else {
                assert!(
                    matches!(run.as_slice(), [Some(dst)] if dst.starts_with("app/src/big.rs#")),
                    "kept read: {run:?}"
                );
            }
        }
        let full = index(
            tempfile::tempdir().unwrap().path(),
            &shadowing(past_the_cap),
        );
        assert_eq!(targets(&full, "util::run"), [None], "read unread");
    }

    // D24, the language half: no rule file reads `World::unread`, so a
    // Ruby file past the cap that also defines `helper` left the other
    // definition the only one, and the call resolved to it.
    #[test]
    fn c10_a_language_with_a_file_indexed_unread_resolves_none_of_its_sites() {
        let big = format!(
            "def helper\n  :big\nend\n{}",
            "# padding\n".repeat(crate::tree::SIZE_CAP as usize / 10 + 1)
        );
        let read = owned(&[
            ("lib/a.rb", "def helper\n  :a\nend\n"),
            (
                "lib/c.rb",
                "require_relative \"a\"\n\nclass C\n  def run\n    helper\n  end\nend\n",
            ),
        ]);
        let resolved = index(tempfile::tempdir().unwrap().path(), &read);
        assert_eq!(
            targets(&resolved, "helper"),
            [Some("lib/a.rb#helper#function")]
        );
        let mut unread = read.clone();
        unread.push(("lib/big.rb".to_owned(), big));
        let rows = index(tempfile::tempdir().unwrap().path(), &unread);
        let sites: Vec<&Edge> = rows.iter().flat_map(|r| &r.edges).collect();
        assert_eq!(sites.len(), 2, "the require and the call");
        assert!(sites.iter().all(|e| e.dst.is_none()), "{sites:?}");
    }

    /// The `dst` of every site of `rows` written `target`.
    fn targets<'a>(rows: &'a [FileRows], target: &str) -> Vec<Option<&'a str>> {
        rows.iter()
            .flat_map(|r| &r.edges)
            .filter(|e| e.target == target)
            .map(|e| e.dst.as_deref())
            .collect()
    }

    /// Ruby and Rust, each with a call whose target a late file decides:
    /// `helper`, which `lib/a.rb` and `zz/b.rb` both define, so a full index
    /// leaves it unresolved, and `s.name()`, which `src/zdef.rs`'s inherent
    /// method takes from the trait's in `src/tr.rs` (D02). Without
    /// `zz/b.rb`, `helper` resolves to `lib/a.rb`, though Ruby calls
    /// `zz/b.rb`'s; without `src/zdef.rs`, `s.name()` resolves to the
    /// trait's method, though rustc calls the inherent one.
    fn shadowed() -> Vec<(String, String)> {
        owned(&[
            ("Cargo.toml", "[package]\nname = \"brs\"\n"),
            ("lib/a.rb", "def helper\n  :a\nend\n"),
            (
                "lib/c.rb",
                "require_relative \"a\"\nrequire_relative \"d\"\nrequire_relative \"../zz/b\"\n\nclass C\n  def run\n    helper\n    other\n  end\nend\n",
            ),
            ("lib/d.rb", "def other\n  :d\nend\n"),
            (
                "src/caller.rs",
                "use crate::{Named, S};\n\npub fn run(s: S) -> u32 {\n    s.name()\n}\n",
            ),
            (
                "src/lib.rs",
                "pub mod caller;\npub mod tr;\npub mod zdef;\n\npub struct S;\n\npub trait Named {\n    fn name(&self) -> u32;\n}\n",
            ),
            (
                "src/tr.rs",
                "use crate::{Named, S};\n\nimpl Named for S {\n    fn name(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/zdef.rs",
                "use crate::S;\n\nimpl S {\n    pub fn name(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            ("zz/b.rb", "def helper\n  :b\nend\n"),
        ])
    }

    /// The edges of the files of `rows` in `lang`, or in every other
    /// language when `of` is false.
    fn edges_of(rows: &[FileRows], lang: Lang, of: bool) -> Vec<Edge> {
        rows.iter()
            .filter(|r| (r.file.lang == lang) == of)
            .flat_map(|r| r.edges.iter().cloned())
            .collect()
    }

    /// From the store holding `previous`, built with `extractor`, runs over
    /// `tree` until `late` is read: a run that reads every planned file but
    /// `late`, one that reads none, then one with no deadline. The first two
    /// leave `late` out of the generation, resolve no site of its language,
    /// decide every other language's sites as `full` (the full index of
    /// `tree`) does, and store the same rows under the same id; the last
    /// stores `full`. Returns the first run's delta.
    fn left_out_until_read(
        tree: &Tree,
        previous: &[FileRows],
        extractor: &str,
        late: &str,
        full: &[FileRows],
    ) -> Delta {
        let lang = lang::wanted(late).expect("a supported file");
        assert!(
            edges_of(full, lang, true).iter().any(|e| e.dst.is_some()),
            "{late}: a full index resolves a site of its language"
        );
        let mut stored = previous.to_vec();
        let mut extractor = extractor.to_owned();
        let mut left_stale = Vec::new();
        let mut first: Option<(Vec<FileRows>, Delta)> = None;
        for reads_the_others in [true, false] {
            let indexed: Vec<FileRow> = stored.iter().map(|r| r.file.clone()).collect();
            let plan = plan(tree, &indexed, Some(&extractor), &left_stale);
            let skipped: Vec<&str> = if reads_the_others {
                vec![late]
            } else {
                plan.extract.iter().map(|f| f.path.as_str()).collect()
            };
            let (now, delta) = run(tree, &plan, &stored, &skipped);
            assert_eq!(delta.stale, [late], "{late}");
            assert!(delta.files.iter().all(|f| f.path != late), "{late}");
            assert_eq!(delta.provenance.extractor, lang::extractor());
            let blind = edges_of(&now, lang, true);
            assert!(!blind.is_empty(), "{late}");
            assert!(blind.iter().all(|e| e.dst.is_none()), "{late}: {blind:?}");
            assert_eq!(edges_of(&now, lang, false), edges_of(full, lang, false));
            match &first {
                None => first = Some((now.clone(), delta.clone())),
                Some((rows, delta_1)) => {
                    assert_eq!(&now, rows, "{late}: a run that reads nothing");
                    assert_eq!(delta.provenance.generation, delta_1.provenance.generation);
                }
            }
            extractor = delta.provenance.extractor.clone();
            left_stale = delta.stale.clone();
            stored = now;
        }
        let indexed: Vec<FileRow> = stored.iter().map(|r| r.file.clone()).collect();
        let next = plan(tree, &indexed, Some(&extractor), &left_stale);
        let read: Vec<&str> = next.extract.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(read, [late]);
        let (unbounded, _) = run(tree, &next, &stored, &[]);
        assert_eq!(unbounded, full, "{late}");
        first.expect("a first run").1
    }

    // IDX-R4-1: during an extractor upgrade, a file the deadline left stale
    // lost its rows, another version's, and the sites of the files read in
    // time were decided without it: a name it also defines resolved to the
    // other file. IDX-F1-1: the next run, which found the file new, did the
    // same, even one that read nothing.
    #[test]
    fn c10_a_stale_file_whose_rows_go_leaves_every_site_of_its_language_unresolved() {
        let files = shadowed();
        let dir = tempfile::tempdir().unwrap();
        let full = index(dir.path(), &files);
        assert_eq!(targets(&full, "helper"), [None], "two files define it");
        let tree = write_tree(dir.path(), &files);
        let other = |lang: Lang| {
            lang::RULES
                .iter()
                .map(|rules| {
                    let version = rules.version() + u32::from(rules.lang() == lang);
                    format!("{}.{version}", rules.lang())
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        for late in ["zz/b.rb", "src/zdef.rs"] {
            let lang = lang::wanted(late).unwrap();
            left_out_until_read(&tree, &full, &other(lang), late, &full);
            // Another language's reading of the file goes the same way, and
            // leaves the sites of the files kept, the caller's among them.
            let mut moved = full.clone();
            for rows in moved.iter_mut().filter(|r| r.file.path == late) {
                rows.file.lang = if lang == Lang::Ruby {
                    Lang::Rust
                } else {
                    Lang::Ruby
                };
            }
            left_out_until_read(&tree, &moved, &lang::extractor(), late, &full);
        }
    }

    // IDX-F1-2: a new file the deadline left stale was absent, and the sites
    // of its language decided without it: a name it also defines resolved
    // to the other file, whether the caller was read in the run or kept.
    // IDX-F1-3: that generation had the id of the full index of the same
    // files, whose edges differ.
    #[test]
    fn c10_a_new_file_left_stale_leaves_every_site_of_its_language_unresolved() {
        let after = shadowed();
        let full = index(tempfile::tempdir().unwrap().path(), &after);
        for (late, names_it) in [
            ("zz/b.rb", "require_relative \"../zz/b\"\n"),
            ("src/zdef.rs", "pub mod zdef;\n"),
        ] {
            let without: Vec<(String, String)> =
                after.iter().filter(|(p, _)| p != late).cloned().collect();
            // The file that names it written with it, not before.
            let first_draft: Vec<(String, String)> = without
                .iter()
                .map(|(path, content)| (path.clone(), content.replace(names_it, "")))
                .collect();
            assert_ne!(first_draft, without);
            for (before, kept) in [(without, true), (first_draft, false)] {
                let dir = tempfile::tempdir().unwrap();
                let previous = index(dir.path(), &before);
                for (path, _) in &before {
                    std::fs::remove_file(dir.path().join(path)).expect("removed");
                }
                let tree = write_tree(dir.path(), &after);
                let blind = left_out_until_read(&tree, &previous, &lang::extractor(), late, &full);
                if kept {
                    // Every file but the new one is kept as it was.
                    let files: Vec<FileRow> = previous.iter().map(|r| r.file.clone()).collect();
                    assert_eq!(blind.files, files, "{late}");
                    assert_ne!(
                        blind.provenance.generation,
                        generation_id(&files, &lang::extractor(), &[]),
                        "{late}: the same files as the full index of `before`, other edges"
                    );
                }
            }
        }
    }

    /// After `delta`, which left a file stale and its language unresolved,
    /// the tree loses that file: what is on disk becomes `without`. The
    /// next run, with nothing to read or drop, must store the full index of
    /// `without` under its id, as a run from nothing does.
    fn decided_again_once_gone(
        root: &Path,
        stored: &[FileRows],
        delta: &Delta,
        without: &[(String, String)],
    ) {
        let late = &delta.stale;
        assert_eq!(late.len(), 1);
        std::fs::remove_file(root.join(&late[0])).expect("removed");
        let tree = write_tree(root, without);
        let plan = plan(
            &tree,
            &delta.files,
            Some(&delta.provenance.extractor),
            &delta.stale,
        );
        assert!(plan.extract.is_empty() && plan.drop.is_empty(), "{late:?}");
        assert!(plan.stale().is_empty(), "{late:?}: counted no more");
        assert!(!plan.is_empty(), "{late:?}: not the full index of the tree");
        let (now, next) = run(&tree, &plan, stored, &[]);
        let full = index(tempfile::tempdir().unwrap().path(), without);
        assert_eq!(now, full, "{late:?}");
        assert!(next.stale.is_empty());
        let files: Vec<FileRow> = full.iter().map(|r| r.file.clone()).collect();
        assert_eq!(next.files, files);
        assert_eq!(
            next.provenance.generation,
            generation_id(&files, &lang::extractor(), &[]),
            "{late:?}"
        );
    }

    // IDX-FINAL2-1: once the stale file that left its language unresolved
    // was gone from the tree, the plan was empty, and the run published the
    // unresolved rows under the id of the full index of the tree, whose
    // rows differ, until some file changed.
    #[test]
    fn c11_a_generation_that_left_a_file_stale_is_decided_again_once_it_is_gone() {
        let after = shadowed();
        let other = |lang: Lang| {
            lang::RULES
                .iter()
                .map(|rules| {
                    let version = rules.version() + u32::from(rules.lang() == lang);
                    format!("{}.{version}", rules.lang())
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        for late in ["zz/b.rb", "src/zdef.rs"] {
            let lang = lang::wanted(late).unwrap();
            let without: Vec<(String, String)> =
                after.iter().filter(|(p, _)| p != late).cloned().collect();
            let full = index(tempfile::tempdir().unwrap().path(), &without);
            assert!(
                edges_of(&full, lang, true).iter().any(|e| e.dst.is_some()),
                "{late}: a full index of the tree without it resolves a site"
            );
            // New, or of an older extractor version, and left stale.
            for (before, extractor) in [(&without, lang::extractor()), (&after, other(lang))] {
                let dir = tempfile::tempdir().unwrap();
                let previous = index(dir.path(), before);
                for (path, _) in before {
                    std::fs::remove_file(dir.path().join(path)).expect("removed");
                }
                let tree = write_tree(dir.path(), &after);
                let indexed: Vec<FileRow> = previous.iter().map(|r| r.file.clone()).collect();
                let plan = plan(&tree, &indexed, Some(&extractor), &[]);
                let (stored, delta) = run(&tree, &plan, &previous, &[late]);
                assert_eq!(delta.stale, [late]);
                let blind = edges_of(&stored, lang, true);
                assert!(blind.iter().all(|e| e.dst.is_none()), "{late}: {blind:?}");
                decided_again_once_gone(dir.path(), &stored, &delta, &without);
            }
        }
    }

    // The previous rows of a file are another extractor's reading when its
    // language's version changed: kept under the new extractor= they would
    // never be read again.
    #[test]
    fn c11_a_stale_file_another_extractor_version_read_is_left_out_and_read_next() {
        let files = owned(&[("a.rb", "def a; b; end\n"), ("b.rb", "def b; end\n")]);
        let dir = tempfile::tempdir().unwrap();
        let previous = index(dir.path(), &files);
        let tree = write_tree(dir.path(), &files);
        let indexed: Vec<FileRow> = previous.iter().map(|r| r.file.clone()).collect();
        let rust = lang::rules(Lang::Rust).version();
        let other = format!("ruby.{},rust.{rust}", lang::rules(Lang::Ruby).version() + 1);
        let plan = plan(&tree, &indexed, Some(&other), &[]);
        assert_eq!(plan.drop, ["a.rb", "b.rb"]);
        let mut extracted = extract(dir.path(), &plan.extract[..1], None);
        extracted.stale = vec!["b.rb".to_owned()];
        let delta = link(&tree, &plan, extracted, |drop| {
            Ok::<_, ()>(kept(&previous, drop))
        })
        .unwrap();
        assert_eq!(delta.drop, ["a.rb", "b.rb"]);
        let paths: Vec<&str> = delta.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rb"]);
        let next = super::plan(
            &tree,
            &delta.files,
            Some(&delta.provenance.extractor),
            &delta.stale,
        );
        let read: Vec<&str> = next.extract.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(read, ["b.rb"]);
    }
}
