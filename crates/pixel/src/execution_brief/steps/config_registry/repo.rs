// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The repository as the script reads it: lines of a file through
//! `Path.read_text(errors="replace")`, the tracked inventory with its caps,
//! and the checks a source file passes before it is quoted.

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use super::super::text::{splitlines, universal_newlines};
use super::super::Ctx;

/// A file larger than this many bytes is not read (the script's
/// `SCAN_MAX_FILE_BYTES`).
pub(super) const MAX_FILE_BYTES: u64 = 400 * 1024;
const TRACKED_PATHS_MAX_BYTES: usize = 4 * 1024 * 1024;
const TRACKED_PATHS_MAX_COUNT: usize = 50_000;

pub(super) struct Repo<'a> {
    pub(super) ctx: &'a Ctx<'a>,
    /// `Path(clone).resolve()`.
    pub(super) root: PathBuf,
    lines: RefCell<HashMap<String, Option<Rc<Vec<String>>>>>,
    inventory: OnceCell<Option<HashSet<String>>>,
}

impl<'a> Repo<'a> {
    pub(super) fn new(ctx: &'a Ctx<'a>) -> Self {
        Self {
            ctx,
            root: std::fs::canonicalize(ctx.root()).unwrap_or_else(|_| ctx.root().to_path_buf()),
            lines: RefCell::new(HashMap::new()),
            inventory: OnceCell::new(),
        }
    }

    pub(super) fn timed_out(&self) -> bool {
        self.ctx.timed_out()
    }

    /// `git ls-files -z`, as the brief listed it once.
    pub(super) fn tracked(&self) -> &[String] {
        self.ctx.files().unwrap_or(&[])
    }

    /// The script's `tracked_paths()`: the set of tracked paths, `None` past
    /// 4 MiB of listing or 50 000 paths, or when git could not list them.
    pub(super) fn inventory(&self) -> Option<&HashSet<String>> {
        self.inventory
            .get_or_init(|| {
                let files = self.ctx.files()?;
                let bytes: usize = files.iter().map(|file| file.len() + 1).sum();
                (bytes <= TRACKED_PATHS_MAX_BYTES && files.len() < TRACKED_PATHS_MAX_COUNT)
                    .then(|| files.iter().cloned().collect())
            })
            .as_ref()
    }

    /// The lines of `rel` (`read_text(errors="replace").splitlines()`); the
    /// file may be a link, as in the script.
    pub(super) fn lines(&self, rel: &str) -> Option<Rc<Vec<String>>> {
        if let Some(cached) = self.lines.borrow().get(rel) {
            return cached.clone();
        }
        let read = std::fs::read(self.root.join(rel)).ok().map(|raw| {
            let text = universal_newlines(&String::from_utf8_lossy(&raw));
            Rc::new(splitlines(&text).into_iter().map(ToString::to_string).collect())
        });
        self.lines.borrow_mut().insert(rel.to_owned(), read.clone());
        read
    }

    /// The resolved path and size of a tracked source file the script would
    /// read: not a link, resolving inside the repository, a regular file of at
    /// most [`MAX_FILE_BYTES`].
    pub(super) fn checked(&self, rel: &str) -> Option<(PathBuf, u64)> {
        let full = self.root.join(rel);
        if std::fs::symlink_metadata(&full).ok()?.file_type().is_symlink() {
            return None;
        }
        let resolved = std::fs::canonicalize(&full).ok()?;
        if !resolved.starts_with(&self.root) {
            return None;
        }
        let meta = std::fs::metadata(&resolved).ok()?;
        (meta.is_file() && meta.len() <= MAX_FILE_BYTES).then_some((resolved, meta.len()))
    }

    /// The text of such a file (`read_text(errors="replace")`).
    pub(super) fn source_text(&self, rel: &str) -> Option<String> {
        let (resolved, _) = self.checked(rel)?;
        let raw = std::fs::read(resolved).ok()?;
        Some(universal_newlines(&String::from_utf8_lossy(&raw)))
    }
}

/// `Path.resolve()` of a path that may not exist: each component resolved in
/// turn, `..` taking the physical parent.
pub(super) fn resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => {
                out.push(other);
                if let Ok(real) = std::fs::canonicalize(&out) {
                    out = real;
                }
            }
        }
    }
    out
}
