// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The search the scripts did with `rg`: every non-hidden, non-ignored file of
//! the working tree, in the order `rg --sort path` walks it (path component by
//! component), searched line by line. Binary files (a NUL byte) are skipped.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;

use ignore::WalkBuilder;

use super::Ctx;
use super::text::splitlines_owned;

/// Bytes of a file that is searched; `rg` has no limit, a generated bundle
/// must not stop the walk.
const MAX_SEARCH_BYTES: u64 = 16_777_216; // 16 MiB
/// Bytes of a file whose lines are quoted.
const MAX_READ_BYTES: u64 = 67_108_864; // 64 MiB
/// Bytes of searched text kept for the next search of the run.
const MAX_CACHED_BYTES: usize = 100_663_296; // 96 MiB

/// A line that matched: the 1-based line as `rg -n` prints it.
#[derive(Clone)]
pub(super) struct Hit {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
}

/// The lines of a searched file, and the same lines lowercased once asked.
pub(super) struct Doc {
    rows: Vec<String>,
    lower: OnceCell<Vec<String>>,
}

impl Doc {
    fn lower(&self) -> &[String] {
        self.lower
            .get_or_init(|| self.rows.iter().map(|row| row.to_lowercase()).collect())
    }
}

pub(super) struct Walk<'a, 'b> {
    ctx: &'a Ctx<'b>,
    paths: Option<Vec<String>>,
    docs: HashMap<String, Option<Rc<Doc>>>,
    cached: usize,
    /// Where `cached` stops growing; a field so a test sets a small one.
    cache_cap: usize,
    lines: HashMap<String, Rc<Vec<String>>>,
}

impl<'a, 'b> Walk<'a, 'b> {
    pub(super) fn new(ctx: &'a Ctx<'b>) -> Self {
        Self {
            ctx,
            paths: None,
            docs: HashMap::new(),
            cached: 0,
            cache_cap: MAX_CACHED_BYTES,
            lines: HashMap::new(),
        }
    }

    /// The lines of `path` as the scripts' `read` gives them (`splitlines`);
    /// empty when it cannot be read.
    pub(super) fn lines(&mut self, path: &str) -> Rc<Vec<String>> {
        if let Some(found) = self.lines.get(path) {
            return Rc::clone(found);
        }
        let rows = Rc::new(
            self.ctx
                .read(path, MAX_READ_BYTES)
                .map_or_else(Vec::new, |text| splitlines_owned(&text)),
        );
        self.lines.insert(path.to_owned(), Rc::clone(&rows));
        rows
    }

    /// The files of the tree, in walking order. `None` when the clock ran out.
    fn walk(&mut self) -> Option<Vec<String>> {
        if let Some(paths) = &self.paths {
            return Some(paths.clone());
        }
        let root = self.ctx.root();
        let walker = WalkBuilder::new(root).sort_by_file_path(Ord::cmp).build();
        let mut paths = Vec::new();
        for entry in walker.flatten() {
            if self.ctx.timed_out() {
                return None;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if let Some(relative) = entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|path| path.to_str())
            {
                paths.push(relative.to_owned());
            }
        }
        self.paths = Some(paths.clone());
        Some(paths)
    }

    /// The files whose name ends in one of `exts`.
    pub(super) fn files(&mut self, exts: &[&str]) -> Option<Vec<String>> {
        Some(
            self.walk()?
                .into_iter()
                .filter(|path| exts.iter().any(|ext| path.ends_with(ext)))
                .collect(),
        )
    }

    fn doc(&mut self, path: &str) -> Option<Rc<Doc>> {
        if let Some(found) = self.docs.get(path) {
            return found.clone();
        }
        let text = self
            .ctx
            .read(path, MAX_SEARCH_BYTES)
            .filter(|text| !text.contains('\0'));
        let found = text.as_ref().map(|text| {
            let mut rows: Vec<String> = text
                .split('\n')
                .map(|row| row.strip_suffix('\r').unwrap_or(row).to_owned())
                .collect();
            if text.ends_with('\n') {
                rows.pop();
            }
            Rc::new(Doc {
                rows,
                lower: OnceCell::new(),
            })
        });
        let size = text.map_or(0, |text| text.len());
        if self.cached + size <= self.cache_cap {
            self.cached += size;
            self.docs.insert(path.to_owned(), found.clone());
        }
        found
    }

    /// The lines of the files ending in `exts` that `matches(path, line)`,
    /// at most `limit` of them. `None` when the clock ran out.
    pub(super) fn grep(
        &mut self,
        exts: &[&str],
        limit: usize,
        matches: &dyn Fn(&str, &str) -> bool,
    ) -> Option<Vec<Hit>> {
        let mut hits = Vec::new();
        for path in self.files(exts)? {
            if self.ctx.timed_out() {
                return None;
            }
            let Some(doc) = self.doc(&path) else {
                continue;
            };
            for (i, row) in doc.rows.iter().enumerate() {
                if matches(&path, row) {
                    hits.push(Hit {
                        path: path.clone(),
                        line: i + 1,
                        text: row.clone(),
                    });
                    if hits.len() >= limit {
                        return Some(hits);
                    }
                }
            }
        }
        Some(hits)
    }

    /// `rg -i -F needle`: the lines that hold `needle` (lowercase) in any case.
    pub(super) fn grep_ci(
        &mut self,
        exts: &[&str],
        limit: usize,
        needle: &str,
    ) -> Option<Vec<Hit>> {
        let mut hits = Vec::new();
        for path in self.files(exts)? {
            if self.ctx.timed_out() {
                return None;
            }
            let Some(doc) = self.doc(&path) else {
                continue;
            };
            for (i, row) in doc.lower().iter().enumerate() {
                if row.contains(needle) {
                    hits.push(Hit {
                        path: path.clone(),
                        line: i + 1,
                        text: doc.rows[i].clone(),
                    });
                    if hits.len() >= limit {
                        return Some(hits);
                    }
                }
            }
        }
        Some(hits)
    }

    /// `rg -c -i -F needle`, summed: the lines that hold `needle` (lowercase) in any case.
    pub(super) fn count_ci(&mut self, exts: &[&str], needle: &str) -> Option<usize> {
        let mut total = 0;
        for path in self.files(exts)? {
            if self.ctx.timed_out() {
                return None;
            }
            if let Some(doc) = self.doc(&path) {
                total += doc
                    .lower()
                    .iter()
                    .filter(|row| row.contains(needle))
                    .count();
            }
        }
        Some(total)
    }

    /// `rg -l`: the first `limit` files with a line that `matches`.
    pub(super) fn files_matching(
        &mut self,
        exts: &[&str],
        limit: usize,
        matches: &dyn Fn(&str) -> bool,
    ) -> Option<Vec<String>> {
        let mut found = Vec::new();
        for path in self.files(exts)? {
            if self.ctx.timed_out() {
                return None;
            }
            if let Some(doc) = self.doc(&path)
                && doc.rows.iter().any(|row| matches(row))
            {
                found.push(path);
                if found.len() >= limit {
                    break;
                }
            }
        }
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn the_byte_limits_are_16_64_and_96_mebibytes() {
        assert_eq!(MAX_SEARCH_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_READ_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_CACHED_BYTES, 96 * 1024 * 1024);
    }

    #[test]
    fn a_searched_file_is_kept_while_the_cache_cap_is_not_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        // Sizes against a cap of 10: 6 fits (6 held), 6 does not (12), 4 fits
        // exactly (10 held), 1 does not (11).
        for (name, text) in [("a", "aaaaaa"), ("b", "bbbbbb"), ("c", "cccc"), ("d", "d")] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let ctx = Ctx::new(
            dir.path(),
            "p",
            "b",
            Instant::now() + Duration::from_secs(30),
        );
        let mut walk = Walk::new(&ctx);
        walk.cache_cap = 10;
        let kept = |walk: &mut Walk, name: &str| {
            let first = walk.doc(name).unwrap();
            let second = walk.doc(name).unwrap();
            Rc::ptr_eq(&first, &second)
        };
        assert!(kept(&mut walk, "a"));
        assert!(!kept(&mut walk, "b"), "6 + 6 is over the cap");
        assert!(kept(&mut walk, "c"), "6 + 4 is exactly the cap");
        assert!(!kept(&mut walk, "d"), "10 + 1 is over the cap");
    }
}
