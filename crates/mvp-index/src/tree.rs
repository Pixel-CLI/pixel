// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The tree to index: the files git lists with a supported language, each
//! with its content hash, however large; the commit and whether the tree is
//! dirty (`design/architecture.md`, "Index generations").

use std::fmt;
use std::fs::Metadata;
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use mvp_model::hash::{Hash, Hasher};
use mvp_model::{Diff, DiffFile, FileStat, Lang};

use crate::guard::{GENERATED_MIN_BYTES, is_generated_blob};
use crate::lang;

/// Pixel's state directory at the root of a repository: never indexed,
/// never counted as changed, whatever git says of it (the plugins a team
/// commits there are not the repository's code).
pub const STATE: &str = ".pixel-mvp";

/// The pathspec that leaves [`STATE`] out of what git lists, so git does
/// not even walk it: `git status`, `ls-files` and `diff` take it after
/// their `--`. The Rust filters ([`in_state`]) stay, for what git lists
/// anyway.
pub const EXCLUDE_STATE: &str = ":(exclude,top).pixel-mvp";

/// The variables that would make git read [`EXCLUDE_STATE`] as a file
/// name: cleared for every git command Pixel runs.
const PATHSPEC_ENV: [&str; 4] = [
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// Whether `path`, relative to the root and `/`-separated, is in
/// [`STATE`].
pub fn in_state(path: &str) -> bool {
    path.strip_prefix(STATE)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Files above this size are indexed unread, never parsed (1 MiB, guessed
/// in `design/architecture.md`).
pub const SIZE_CAP: u64 = 1 << 20;

/// A working tree as git sees it.
#[derive(Clone, Debug)]
pub struct Tree {
    /// The top level of the work tree.
    pub root: PathBuf,
    /// `HEAD`, or `None` before the first commit.
    pub commit: Option<String>,
    /// Whether the tree differs from `commit`: a change `git status`
    /// reports, whatever the configuration hides (untracked files,
    /// submodules, stat checks); an edit to a file the index reads that git
    /// was told to overlook (skip-worktree, assume-unchanged), or its
    /// removal, unless a sparse checkout leaves it out; or a file or `HEAD`
    /// that changed while the scan ran.
    pub dirty: bool,
    /// The files to index, sorted by path.
    pub files: Vec<TreeFile>,
    /// Files with a supported language that could not be read, by reason.
    pub skipped: Vec<Skipped>,
    /// The files git lists as untracked and not ignored, in any language,
    /// outside Pixel's state: what [`changes`] counts whole.
    pub untracked: Vec<String>,
    /// The copy of git's index the scan read in its place, when it was of
    /// git's index as the scan found it ([`git_index`](crate::git_index)).
    pub index: Option<PathBuf>,
    /// Git's index has no refreshed copy yet: the writer makes one.
    pub unsettled: bool,
}

/// One file to index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// Relative to the root, `/`-separated.
    pub path: String,
    pub lang: Lang,
    pub hash: Hash,
    pub bytes: u64,
    /// The stat the hash was read under; `None` when the file changed in
    /// the scan's own second (git's racy-clean case) or the platform gives
    /// none: such a file is hashed again by the next scan (ADR-0022 §2).
    pub stat: Option<FileStat>,
}

/// A file a language would read but the tree scan could not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Listed by git but not a regular file on disk: deleted, a symlink, a
    /// submodule.
    Absent,
    /// A regular file whose read failed.
    Unreadable,
}

/// Whether a file of `lang` is indexed without being parsed: larger than
/// [`SIZE_CAP`], or a generated blob its rules refuse. Its definitions are
/// unknown, so it is read by [`Rules::unread`](crate::lang::Rules::unread),
/// not `extract`. A function of the bytes alone, asked about the bytes
/// extraction reads, never the scan's (`extract::rows`): the rows and the
/// hash stored with them always come from the same bytes.
pub fn unread(lang: Lang, path: &str, content: &[u8]) -> bool {
    content.len() as u64 > SIZE_CAP
        || (!lang::accepts(lang, path, content) && is_generated_blob(content))
}

#[derive(Debug)]
pub enum TreeError {
    /// The path is not inside a git work tree, or git is not installed.
    NotAWorkTree { path: PathBuf, detail: String },
    /// A git command failed.
    Git { command: String, detail: String },
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::NotAWorkTree { path, detail } => {
                write!(
                    f,
                    "{} is not inside a git work tree: {detail}",
                    path.display()
                )
            }
            TreeError::Git { command, detail } => write!(f, "`{command}` failed: {detail}"),
        }
    }
}

impl std::error::Error for TreeError {}

/// `git status`, with what it reports spelled out, so that
/// `status.showUntrackedFiles`, `diff.ignoreSubmodules`, `core.trustctime`
/// or `core.checkStat` cannot hide a change (C-11).
const STATUS: &[&str] = &[
    "-c",
    "core.trustctime=true",
    "-c",
    "core.checkStat=default",
    "status",
    "--porcelain",
    "-z",
    "--untracked-files=all",
    "--ignore-submodules=none",
    "--",
    EXCLUDE_STATE,
];

/// Scans the work tree that contains `path`. The tracked files are read
/// before git is asked whether the tree is dirty, and every file's metadata
/// is read again after: a save or a checkout that lands during the scan
/// shows in `git status`, moves `HEAD`, or changes a file under the scan,
/// and the tree is dirty in each case (C-11). The untracked files are the
/// ones `git status` lists, read after it: any one of them already makes
/// the tree dirty. Walking the tree for them once, not twice, is ~80 ms a
/// prompt on a 19k-file tree.
pub fn scan(path: &Path) -> Result<Tree, TreeError> {
    scan_with(path, &|_| None)
}

/// [`scan`], taking the hash of a file whose stat is the one `known` gives
/// with it instead of reading the file (ADR-0022 §2): on a 19k-file tree,
/// reading and hashing every file is ~70 ms of each prompt.
pub fn scan_with(path: &Path, known: &Known<'_>) -> Result<Tree, TreeError> {
    scan_discovered(discover(path)?, known)
}

/// Scans a previously discovered work tree, consuming its initial HEAD.
/// The final HEAD and metadata checks still detect a checkout or save
/// between discovery and the end of the scan (C-11).
pub fn scan_discovered(repo: Repository, known: &Known<'_>) -> Result<Tree, TreeError> {
    let base = repo.root.clone();
    scan_discovered_in(repo, known, &base)
}

/// [`scan_discovered`], reading the copy of git's index (ADR-0039) from
/// `base`'s [`STATE`] instead of the work tree root's: `base` is the
/// directory a caller gives the store (`Writer::open`, `place_git_index`),
/// when it keeps Pixel's state away from the root (v1 keeps it in
/// `.pixel/.pixel-mvp/`, so its `base` is `<root>/.pixel`). The copy is
/// still keyed by git's index file and read only while that holds.
pub fn scan_discovered_in(
    repo: Repository,
    known: &Known<'_>,
    base: &Path,
) -> Result<Tree, TreeError> {
    let Repository {
        root,
        commit,
        index,
    } = repo;
    let copy = index
        .as_deref()
        .and_then(|source| crate::git_index::Copy::of(base, source));
    let unsettled = copy.as_ref().is_some_and(|copy| !copy.made());
    // The copy stands for git's index only while that is the version it
    // was made of, the scan's end included; else git's own index is read.
    if let Some(copy) = copy.filter(crate::git_index::Copy::made)
        && let Ok(tree) = scan_using(&root, commit.clone(), Some(copy.path()), known)
        && copy.holds()
    {
        return Ok(tree);
    }
    let mut tree = scan_using(&root, commit, None, known)?;
    tree.unsettled = unsettled;
    Ok(tree)
}

/// [`scan_discovered`], git reading `index` in place of its own index file
/// when it is given.
pub(crate) fn scan_using(
    root: &Path,
    commit: Option<String>,
    index: Option<&Path>,
    known: &Known<'_>,
) -> Result<Tree, TreeError> {
    let start = now_secs();
    let root = root.to_path_buf();
    let listed = git_with(
        &root,
        index,
        &[
            "ls-files",
            "-z",
            "-v",
            "-s",
            "--cached",
            "--",
            EXCLUDE_STATE,
        ],
        &[],
    )?;
    // A path that is not UTF-8 is not indexed, so it is left out here.
    let mut tracked = wanted_of(
        listed
            .split(|byte| *byte == 0)
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(entry),
    );
    let read_tracked = each(&tracked, |file| read_one(&root, file, known, start));
    let status = git_with(&root, index, STATUS, &[])?;
    let untracked: Vec<String> = untracked_of(&status)
        .into_iter()
        .filter(|path| !in_state(path))
        .map(str::to_owned)
        .collect();
    let others = wanted_of(untracked.iter().map(|path| (path.as_str(), None)));
    let read_others = each(&others, |file| read_one(&root, file, known, start));
    let mut both: Vec<(Listed<'_>, (Read, Option<Stamp>))> = tracked
        .drain(..)
        .zip(read_tracked)
        .chain(others.into_iter().zip(read_others))
        .collect();
    both.sort_by(|(a, _), (b, _)| a.path.cmp(b.path));
    both.dedup_by(|(a, _), (b, _)| a.path == b.path);
    let (wanted, read): (Vec<Listed<'_>>, Vec<(Read, Option<Stamp>)>) = both.into_iter().unzip();
    let dirty = outside_state(&status)
        || head(&root) != commit
        || differs(&root, &wanted, &read)
        || each(&wanted, |file| stamp(&root.join(file.path)))
            .iter()
            .zip(&read)
            .any(|(now, (_, then))| now != then);
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for (outcome, _) in read {
        match outcome {
            Read::File(file) => files.push(file),
            Read::Skipped(skip) => skipped.push(skip),
            Read::Foreign => {}
        }
    }
    Ok(Tree {
        root,
        commit,
        dirty,
        files,
        skipped,
        untracked,
        index: index.map(Path::to_path_buf),
        unsettled: false,
    })
}

/// The working tree's changes against `HEAD`, for the chain's step 3
/// (`design/minimap-selection.md`): the lines of each hunk as the new file
/// numbers them, and each untracked file as a whole. Before the first
/// commit every file is untracked or staged, and counts as a whole.
pub fn diff(root: &Path) -> Result<Diff, TreeError> {
    let committed = head(root).is_some();
    let files = if committed {
        hunks(root, None)?
    } else {
        Vec::new()
    };
    // Before the first commit, the staged files count whole too.
    let listed: &[&str] = if committed {
        &["--others"]
    } else {
        &["--cached", "--others"]
    };
    let args = [
        &["ls-files"],
        listed,
        &["--exclude-standard", "-z", "--", EXCLUDE_STATE],
    ]
    .concat();
    let whole = git_bytes(root, &args, &[])?;
    let whole = whole
        .split(|b| *b == 0)
        .filter_map(|p| std::str::from_utf8(p).ok());
    Ok(changed(files, whole))
}

/// [`diff`] of the tree `scan` read, without listing the tree again: no
/// change when the scan found it as `HEAD` holds it (`git status` clean,
/// nothing overlooked), else the hunks against `HEAD` and the untracked
/// files the scan listed. On `antiwork/gumroad` (12k files) this is 130 ms
/// less per prompt than [`diff`], whose own listing walks the whole tree.
/// Before the first commit it is [`diff`].
pub fn changes(tree: &Tree) -> Result<Diff, TreeError> {
    if tree.commit.is_none() {
        return diff(&tree.root);
    }
    if !tree.dirty {
        return Ok(Diff::default());
    }
    // The copy of git's index gone since the scan: git's own is read.
    let files = match tree.index.as_deref() {
        Some(index) => hunks(&tree.root, Some(index)).or_else(|_| hunks(&tree.root, None))?,
        None => hunks(&tree.root, None)?,
    };
    Ok(changed(files, tree.untracked.iter().map(String::as_str)))
}

/// The hunks of `git diff HEAD`, each file with the lines its hunks hold.
fn hunks(root: &Path, index: Option<&Path>) -> Result<Vec<DiffFile>, TreeError> {
    let mut files: Vec<DiffFile> = Vec::new();
    // Quoted paths keep the output ASCII, so a name is read back byte
    // for byte; the prefixes are set whatever `diff.noprefix` or
    // `diff.mnemonicPrefix` say.
    let out = git_with(
        root,
        index,
        &[
            "-c",
            "core.quotePath=true",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-U0",
            "HEAD",
            "--",
            EXCLUDE_STATE,
        ],
        &[],
    )
    .map(|out| String::from_utf8_lossy(&out).into_owned())?;
    let mut current: Option<usize> = None;
    // Lines of the current hunk still to come, old and new: a removed
    // or added line may itself read `+++ b/x`.
    let (mut old, mut new) = (0u32, 0u32);
    for line in out.split('\n') {
        if old > 0 || new > 0 {
            match line.as_bytes().first() {
                Some(b'-') if old > 0 => old -= 1,
                Some(b'+') if new > 0 => new -= 1,
                Some(b'\\') => {}
                _ => (old, new) = (0, 0),
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            current = new_path(path).map(|path| {
                files.push(DiffFile {
                    path,
                    hunks: Vec::new(),
                });
                files.len() - 1
            });
        } else if let Some(header) = line.strip_prefix("@@ ") {
            (old, new) = lengths(header).unwrap_or((0, 0));
            if let (Some(at), Some(hunk)) = (current, hunk(header)) {
                files[at].hunks.push(hunk);
            }
        }
    }
    Ok(files)
}

/// `files`, with each of `whole` as a whole file, outside Pixel's state,
/// sorted and once each.
fn changed<'a>(mut files: Vec<DiffFile>, whole: impl Iterator<Item = &'a str>) -> Diff {
    for path in whole {
        if !path.is_empty() {
            files.push(DiffFile {
                path: path.to_owned(),
                hunks: vec![(1, u32::MAX)],
            });
        }
    }
    files.retain(|file| !file.hunks.is_empty() && !in_state(&file.path));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    Diff { files }
}

/// The files of `entries` a language reads, outside Pixel's state, sorted
/// and once each.
fn wanted_of<'a>(entries: impl Iterator<Item = (&'a str, Option<Cached<'a>>)>) -> Vec<Listed<'a>> {
    let mut wanted: Vec<Listed<'a>> = entries
        .filter(|(path, _)| !in_state(path))
        .filter_map(|(path, cached)| {
            Some(Listed {
                path,
                lang: lang::wanted(path)?,
                cached,
            })
        })
        .collect();
    wanted.sort_by(|a, b| a.path.cmp(b.path));
    wanted.dedup_by(|a, b| a.path == b.path);
    wanted
}

/// The untracked files `git status --porcelain -z` lists (`?? path`); a
/// path that is not UTF-8 is left out, as it is not indexed.
fn untracked_of(status: &[u8]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut fields = status.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        if matches!(entry.first(), Some(b'R' | b'C')) {
            fields.next();
        } else if let Some(path) = entry.strip_prefix(b"?? ") {
            out.extend(
                std::str::from_utf8(path)
                    .ok()
                    .filter(|path| !path.is_empty()),
            );
        }
    }
    out
}

/// Whether `git status --porcelain -z` lists a change outside [`STATE`]:
/// each entry is `XY <path>`, a rename or a copy followed by its source.
fn outside_state(status: &[u8]) -> bool {
    let mut fields = status.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        let moved = matches!(entry.first(), Some(b'R' | b'C'));
        let mut paths = vec![entry.get(3..).unwrap_or_default()];
        if moved {
            paths.extend(fields.next());
        }
        if paths
            .iter()
            .any(|path| std::str::from_utf8(path).map_or(true, |path| !in_state(path)))
        {
            return true;
        }
    }
    false
}

/// The new side of a hunk header, `-a,b +c,d @@`: lines `c` to `c+d-1`, or
/// the line after `c` for a deletion (`d` = 0).
fn hunk(header: &str) -> Option<(u32, u32)> {
    let new = header
        .split_whitespace()
        .find_map(|part| part.strip_prefix('+'))?;
    let (start, count) = match new.split_once(',') {
        Some((start, count)) => (start.parse::<u32>().ok()?, count.parse::<u32>().ok()?),
        None => (new.parse::<u32>().ok()?, 1),
    };
    Some(if count == 0 {
        (start + 1, start + 1)
    } else {
        (start.max(1), start + count - 1)
    })
}

/// The lines a hunk header, `-a,b +c,d @@`, says follow it: `b` and `d`.
fn lengths(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace();
    let count = |side: &str| -> Option<u32> {
        match side.split_once(',') {
            Some((_, count)) => count.parse().ok(),
            None => Some(1),
        }
    };
    let old = count(parts.next()?.strip_prefix('-')?)?;
    let new = count(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

/// The path of a `+++ ` header: `b/<path>`, followed by a TAB when the
/// path holds a space, or C-quoted (`"b/a\"b.rb"`, `"b/\303\251.rb"`).
/// `None` for `/dev/null` and for a name that is not UTF-8.
fn new_path(raw: &str) -> Option<String> {
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    let path = match raw.strip_prefix('"') {
        Some(quoted) => unquote(quoted.strip_suffix('"')?)?,
        None => raw.to_owned(),
    };
    path.strip_prefix("b/").map(str::to_owned)
}

/// A name git C-quoted, without its quotes: `\\`, `\"`, `\a\b\t\n\v\f\r`
/// and three octal digits per byte.
fn unquote(quoted: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(quoted.len());
    let mut rest = quoted.bytes();
    while let Some(b) = rest.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        let escaped = match rest.next()? {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            digit @ b'0'..=b'3' => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    let d = rest.next().filter(|d| matches!(d, b'0'..=b'7'))?;
                    value = value * 8 + u32::from(d - b'0');
                }
                u8::try_from(value).ok()?
            }
            other => other,
        };
        bytes.push(escaped);
    }
    String::from_utf8(bytes).ok()
}

/// `HEAD`, or `None` before the first commit.
fn head(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .ok()
        .map(|out| out.trim().to_owned())
        .filter(|sha| !sha.is_empty())
}

/// A file a language would read, as `git ls-files` lists it.
struct Listed<'a> {
    path: &'a str,
    lang: Lang,
    /// Its index entry; `None` for an untracked file.
    cached: Option<Cached<'a>>,
}

#[derive(Clone, Copy)]
struct Cached<'a> {
    /// The blob the index holds.
    blob: &'a str,
    /// A regular file (mode `100644` or `100755`), not a symlink or a
    /// submodule.
    regular: bool,
    overlooked: Option<Overlooked>,
}

/// What git was told about a file on disk: not to look at it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overlooked {
    /// Tag `S` or `s`; also how a sparse checkout leaves a file out.
    SkipWorktree,
    /// A lower-case tag.
    AssumeUnchanged,
}

/// One entry of `git ls-files -z -v -s`: `<tag> <mode> <blob> <stage>\t<path>`
/// for a cached file, `? <path>` for an untracked one.
fn entry(line: &str) -> Option<(&str, Option<Cached<'_>>)> {
    let (tag, rest) = line.split_at_checked(2)?;
    if tag == "? " {
        return Some((rest, None));
    }
    let (stat, path) = rest.split_once('\t')?;
    let mut fields = stat.split(' ');
    let regular = matches!(fields.next()?, "100644" | "100755");
    let blob = fields.next()?;
    let overlooked = if tag.starts_with(['S', 's']) {
        Some(Overlooked::SkipWorktree)
    } else if tag.starts_with(|c: char| c.is_ascii_lowercase()) {
        Some(Overlooked::AssumeUnchanged)
    } else {
        None
    };
    Some((
        path,
        Some(Cached {
            blob,
            regular,
            overlooked,
        }),
    ))
}

/// Whether a file git overlooks differs from its index entry: edited, or
/// gone where the entry is a regular file. After a clean `git status` every
/// index entry is `HEAD`'s, so this compares with `HEAD`.
fn differs(root: &Path, wanted: &[Listed<'_>], read: &[(Read, Option<Stamp>)]) -> bool {
    let mut present = Vec::new();
    let mut gone = Vec::new();
    for (file, (outcome, _)) in wanted.iter().zip(read) {
        let Some(cached) = file.cached.filter(|c| c.overlooked.is_some()) else {
            continue;
        };
        if !outcome.absent() {
            present.push((file.path, cached.blob));
        } else if cached.regular {
            if cached.overlooked == Some(Overlooked::SkipWorktree) {
                gone.push(file.path);
            } else {
                return true;
            }
        }
    }
    removed(root, &gone) || edited(root, &present)
}

/// Whether one of these absent skip-worktree files is one a clean checkout
/// of `HEAD` holds: any of them outside a sparse checkout; inside one, those
/// its patterns keep, as `git sparse-checkout check-rules` says. Where git
/// cannot say (older than 2.42, or no patterns to read), every one is taken
/// as left out by the sparse checkout.
fn removed(root: &Path, gone: &[&str]) -> bool {
    if gone.is_empty() {
        return false;
    }
    let sparse = git(root, &["config", "--bool", "core.sparseCheckout"])
        .is_ok_and(|out| out.trim() == "true");
    if !sparse {
        return true;
    }
    let paths: Vec<u8> = gone
        .iter()
        .flat_map(|path| path.bytes().chain([0]))
        .collect();
    git_bytes(root, &["sparse-checkout", "check-rules", "-z"], &paths)
        .is_ok_and(|kept| !kept.is_empty())
}

/// Whether a file differs from the blob given with it, hashed by git: as it
/// is first, then through the clean filters `git add` would apply (line
/// endings, `filter=`). The first matches a file committed with CRLF that
/// conversion, turned on later, would rewrite; `git status` spares it the
/// same way. Only these paths are hashed; a failure counts as an edit.
fn edited(root: &Path, files: &[(&str, &str)]) -> bool {
    files.chunks(512).any(|chunk| {
        mismatched(root, &["hash-object", "--no-filters", "--"], chunk).is_none_or(|raw| {
            !raw.is_empty()
                && mismatched(root, &["hash-object", "--"], &raw)
                    .is_none_or(|left| !left.is_empty())
        })
    })
}

/// The files whose hash by `command` is not their blob, or `None` when git
/// fails.
fn mismatched<'a>(
    root: &Path,
    command: &[&str],
    files: &[(&'a str, &'a str)],
) -> Option<Vec<(&'a str, &'a str)>> {
    let mut args = command.to_vec();
    args.extend(files.iter().map(|(path, _)| *path));
    let out = git(root, &args).ok()?;
    let hashes: Vec<&str> = out.lines().collect();
    (hashes.len() == files.len()).then(|| {
        files
            .iter()
            .zip(hashes)
            .filter(|((_, blob), hash)| hash != blob)
            .map(|(file, _)| *file)
            .collect()
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Read {
    File(TreeFile),
    Skipped(Skipped),
    /// Not the language's file after all: its rules refuse it for another
    /// reason than its size or being generated (a binstub without a Ruby
    /// shebang).
    Foreign,
}

impl Read {
    /// Nothing regular on disk: a file read, unreadable or of another
    /// language is there.
    fn absent(&self) -> bool {
        matches!(
            self,
            Read::Skipped(Skipped {
                reason: SkipReason::Absent,
                ..
            })
        )
    }
}

/// What a save changes in a file's metadata: its size and modification
/// time, and on Unix its inode and change time, which no tool sets back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    node: (u64, u64, i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Stamp {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            node: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// The stamp of whatever is at `path`, a symlink not followed.
fn stamp(path: &Path) -> Option<Stamp> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| Stamp::of(&meta))
}

/// Runs `f` over the files on every core, keeping their order.
fn each<T: Send>(files: &[Listed<'_>], f: impl Fn(&Listed<'_>) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk = files.len().div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|scope| {
        let workers: Vec<_> = files
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<_>>()))
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a reader thread panicked"))
            .collect()
    })
}

/// The hash and stat a store recorded for a path, if it did.
pub type Known<'a> = dyn Fn(&str) -> Option<(Hash, FileStat)> + Sync + 'a;

/// Reads and hashes one file, with the stamp it had before it was read: a
/// save during the read changes the stamp. A file whose stat is the one
/// `known` recorded is not read: its hash is the recorded one.
fn read_one(
    root: &Path,
    file: &Listed<'_>,
    known: &Known<'_>,
    start: i64,
) -> (Read, Option<Stamp>) {
    let full = root.join(file.path);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return (skipped(file.path, SkipReason::Absent), None);
    };
    let stat = file_stat(&meta);
    if let (true, Some(now), Some((hash, then))) = (meta.is_file(), stat, known(file.path))
        && now == then
    {
        let reused = TreeFile {
            path: file.path.to_owned(),
            lang: file.lang,
            hash,
            bytes: now.len,
            stat: Some(now),
        };
        return (Read::File(reused), Some(Stamp::of(&meta)));
    }
    (
        read_file(&full, &meta, file, settled(stat, start)),
        Some(Stamp::of(&meta)),
    )
}

/// The seconds since the epoch, now.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `stat`, unless the file was written in the scan's own second or later:
/// a write later in that second can leave a coarse clock's times as they
/// were, so that stat is not to be trusted (git's racy-clean rule).
fn settled(stat: Option<FileStat>, start: i64) -> Option<FileStat> {
    let second = |ns: i64| ns.div_euclid(1_000_000_000);
    stat.filter(|stat| second(stat.mtime_ns) < start && second(stat.ctime_ns) < start)
}

/// A file's stat as [`FileStat`] holds it; none off Unix.
fn file_stat(meta: &Metadata) -> Option<FileStat> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
        Some(FileStat {
            len: meta.len(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn read_file(full: &Path, meta: &Metadata, file: &Listed<'_>, stat: Option<FileStat>) -> Read {
    let (path, lang) = (file.path, file.lang);
    if !meta.is_file() {
        return skipped(path, SkipReason::Absent);
    }
    match std::fs::File::open(full).and_then(|file| hash_owned(lang, path, file)) {
        Ok(Some((hash, bytes))) => Read::File(TreeFile {
            path: path.to_owned(),
            lang,
            hash,
            bytes,
            stat,
        }),
        Ok(None) => Read::Foreign,
        Err(_) => skipped(path, SkipReason::Unreadable),
    }
}

fn skipped(path: &str, reason: SkipReason) -> Read {
    Read::Skipped(Skipped {
        path: path.to_owned(),
        reason,
    })
}

/// Whether `lang`'s rules take the file for theirs, its size and shape
/// aside: they are asked about its head, shorter than a generated blob, so
/// a refusal says the file is not the language's (a binstub of another
/// interpreter), never that it is generated (`Rules::accepts`).
fn owns(lang: Lang, path: &str, head: &[u8]) -> bool {
    lang::accepts(lang, path, head)
}

/// The content hash and size of a file `lang` owns ([`owns`]), `None` for
/// one it does not. Only the head is read before that is known, so a large
/// file of another language (a binary in `bin/`) costs its head, never a
/// full read; the rest of an owned file is hashed as it streams by, never
/// held.
fn hash_owned(
    lang: Lang,
    path: &str,
    mut file: impl std::io::Read,
) -> std::io::Result<Option<(Hash, u64)>> {
    let mut head = Vec::new();
    (&mut file)
        .take(GENERATED_MIN_BYTES as u64 - 1)
        .read_to_end(&mut head)?;
    if !owns(lang, path, &head) {
        return Ok(None);
    }
    let mut hasher = Hasher::new();
    hasher.update(&head);
    let mut bytes = head.len() as u64;
    let mut buffer = vec![0; 1 << 16];
    loop {
        let n = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok(Some((hasher.finish(), bytes)))
}

/// A work tree and its initial HEAD, discovered by one git process.
/// Consumed by [`scan_discovered`], never cached between scans.
#[derive(Debug)]
pub struct Repository {
    root: PathBuf,
    commit: Option<String>,
    /// Git's index file, as `git rev-parse --git-path index` names it (a
    /// linked work tree's is under its own git directory), made absolute;
    /// `None` when git's answer could not be told from the root's.
    index: Option<PathBuf>,
}

impl Repository {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Git's index file, when known.
    pub fn index(&self) -> Option<&Path> {
        self.index.as_deref()
    }
}

/// Resolves git's index file, the root and the initial HEAD together, in
/// one git. Git writes the index file's path and the root first, even
/// before the first commit (exit 1); only a successful verification adds
/// a final hash line. Split from the end so newlines in the root are
/// preserved, including a trailing newline in its name; when a newline in
/// a name leaves the index file's path and the root ambiguous, they are
/// asked again without the index file, which the scan then reads as git
/// finds it.
pub fn discover(path: &Path) -> Result<Repository, TreeError> {
    resolve(path, true)
}

fn resolve(path: &Path, with_index: bool) -> Result<Repository, TreeError> {
    let failed = |detail| TreeError::NotAWorkTree {
        path: path.to_path_buf(),
        detail,
    };
    let index_args: &[&str] = if with_index {
        &["--git-path", "index"]
    } else {
        &[]
    };
    let output = git_in(path)
        .arg("rev-parse")
        .args(index_args)
        .args(["--show-toplevel", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|err| failed(err.to_string()))?;
    let detail = || String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let Some(mut bytes) = output.stdout.strip_suffix(b"\n") else {
        return Err(failed(detail()));
    };
    let commit = if output.status.success() {
        let Some(at) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Err(failed(detail()));
        };
        let hash = std::str::from_utf8(&bytes[at + 1..])
            .map_err(|err| failed(err.to_string()))?
            .to_owned();
        bytes = &bytes[..at];
        Some(hash)
    } else if output.status.code() == Some(1) && !bytes.is_empty() {
        None
    } else {
        return Err(failed(detail()));
    };
    let mut index = None;
    if with_index {
        let mut lines = bytes.splitn(2, |b| *b == b'\n');
        let (Some(git_path), Some(rest)) = (lines.next(), lines.next()) else {
            return Err(failed(detail()));
        };
        if rest.contains(&b'\n') || git_path.is_empty() || rest.is_empty() {
            return resolve(path, false);
        }
        // Relative to the directory git ran in; made absolute, and the
        // same whichever directory of the work tree a command starts in.
        index = std::fs::canonicalize(path.join(path_of(git_path))).ok();
        bytes = rest;
    }
    Ok(Repository {
        root: path_of(bytes),
        commit,
        index,
    })
}

/// A path git printed, byte for byte where the platform allows.
fn path_of(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

/// The top level of the work tree that contains `path`.
pub fn root(path: &Path) -> Result<PathBuf, TreeError> {
    let output = git_in(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| TreeError::NotAWorkTree {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(TreeError::NotAWorkTree {
            path: path.to_path_buf(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(PathBuf::from(text.trim_end_matches(['\n', '\r'])))
}

/// Whether the index of the work tree at `root` reads no file under
/// `path`, relative to `root`: git ignores it. [`scan`] reads only the
/// files git lists, tracked or untracked and not ignored, and
/// `check-ignore` does not count as ignored a directory that holds a
/// tracked file. `false` when git cannot tell. The path goes to git as
/// `./<path>`: a name that starts with `:` is pathspec magic otherwise
/// (`:h` asks about `h`), and `check-ignore` refuses `--literal-pathspecs`.
pub fn ignored(root: &Path, path: &Path) -> bool {
    git_in(root)
        .args(["check-ignore", "-q", "--"])
        .arg(Path::new(".").join(path))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Runs git in `root` and returns its stdout as text, bytes that are not
/// UTF-8 replaced: for output whose paths are never looked up on disk.
fn git(root: &Path, args: &[&str]) -> Result<String, TreeError> {
    git_bytes(root, args, &[]).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// Runs git in `root` with `input` on its stdin and returns its stdout.
fn git_bytes(root: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>, TreeError> {
    git_with(root, None, args, input)
}

/// [`git_bytes`], git reading `index` in place of its own index file when
/// it is given.
fn git_with(
    root: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: &[u8],
) -> Result<Vec<u8>, TreeError> {
    let failed = |detail: String| TreeError::Git {
        command: format!("git {}", args.join(" ")),
        detail,
    };
    let mut command = git_in(root);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| failed(err.to_string()))?;
    // Written from another thread, so that git never waits on a full
    // stdout while this one waits on a full stdin.
    let stdin = child.stdin.take().filter(|_| !input.is_empty());
    let output = std::thread::scope(|scope| {
        let writer = stdin.map(|mut stdin| scope.spawn(move || stdin.write_all(input)));
        let output = child.wait_with_output()?;
        if let Some(writer) = writer {
            writer.join().expect("the stdin writer panicked")?;
        }
        Ok::<_, std::io::Error>(output)
    })
    .map_err(|err| failed(err.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output.stdout)
}

/// The variables that point git at a repository other than the one around
/// its working directory, as `git rev-parse --local-env-vars` lists them.
/// A run started from a git hook or from `git rebase --exec` inherits them,
/// and git would then describe that repository's commit and index for the
/// files of the one `path` is in (C-11).
pub const LOCAL_ENV: [&str; 15] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// `git -C dir` on the repository around `dir`, whatever [`LOCAL_ENV`]
/// says, that never takes `.git/index.lock`: a `status` finding the stat
/// cache stale would otherwise rewrite `.git/index`, and the user's own git
/// commands fail while it holds the lock.
pub(crate) fn git_in(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for var in LOCAL_ENV.iter().chain(&PATHSPEC_ENV) {
        command.env_remove(var);
    }
    command.arg("--no-optional-locks").arg("-C").arg(dir);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c11_discovery_keeps_the_initial_head_until_the_scan_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["commit", "--allow-empty", "-qm", "one"]);
        let repo = discover(root).unwrap();
        let initial = repo.commit.clone();
        // The final tree is clean, but a checkout/commit between discovery
        // and the scan must not be labelled clean at the initial HEAD.
        run_git(root, &["commit", "--allow-empty", "-qm", "two"]);
        let tree = scan_discovered(repo, &|_| None).unwrap();
        assert_eq!(tree.commit, initial);
        assert!(tree.dirty);
        assert!(!scan(root).unwrap().dirty);
    }

    #[test]
    fn c11_discovery_distinguishes_unborn_nested_and_non_repositories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path()).is_err());
        run_git(dir.path(), &["init", "-q"]);
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let repo = discover(&nested).unwrap();
        assert_eq!(repo.root(), dir.path().canonicalize().unwrap());
        assert!(repo.commit.is_none());
        let tree = scan_discovered(repo, &|_| None).unwrap();
        assert!(tree.commit.is_none());
        assert!(!tree.dirty);
        run_git(dir.path(), &["commit", "--allow-empty", "-qm", "one"]);
        assert_eq!(discover(&nested).unwrap().commit, head(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn c18_discovery_preserves_root_bytes_and_never_refreshes_the_git_index() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        #[cfg(target_os = "linux")]
        let name = b"root\n\xff\r\n".as_slice();
        #[cfg(not(target_os = "linux"))]
        let name = b"root\n\r\n".as_slice();
        let root = dir.path().join(std::ffi::OsStr::from_bytes(name));
        std::fs::create_dir(&root).unwrap();
        run_git(&root, &["init", "-q"]);
        assert_eq!(
            discover(&root).unwrap().root(),
            root.canonicalize().unwrap()
        );
        std::fs::write(root.join("a.rb"), "def hello; end\n").unwrap();
        run_git(&root, &["add", "a.rb"]);
        run_git(&root, &["commit", "-qm", "one"]);
        let before = std::fs::read(root.join(".git/index")).unwrap();
        let repo = discover(&root).unwrap();
        assert_eq!(repo.root(), root.canonicalize().unwrap());
        let tree = scan_discovered(repo, &|_| None).unwrap();
        assert!(!tree.dirty);
        assert_eq!(tree.files.len(), 1);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before);
    }

    fn listed(path: &str) -> Listed<'_> {
        Listed {
            path,
            lang: Lang::Ruby,
            cached: None,
        }
    }

    fn file(path: &str, content: &[u8]) -> Read {
        Read::File(TreeFile {
            path: path.to_owned(),
            lang: Lang::Ruby,
            hash: Hash::of(content),
            bytes: content.len() as u64,
            stat: None,
        })
    }

    // D24: a file past the cap or a generated blob was left out of the
    // tree, so its definitions were missing from every decision.
    #[test]
    fn a_file_past_the_cap_or_generated_is_listed_and_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let large = "# padding\n".repeat(SIZE_CAP as usize / 10 + 1);
        let blob = format!("X = [{}]\n", "1,".repeat(40_000));
        let script = "#!/bin/sh\necho hi\n";
        for (path, content) in [
            ("lib/a.rb", "def a; end\n"),
            ("lib/big.rb", large.as_str()),
            ("lib/blob.rb", blob.as_str()),
            ("bin/dev", script),
        ] {
            std::fs::create_dir_all(dir.path().join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.path().join(path), content).unwrap();
        }
        let read = |path| read_one(dir.path(), &listed(path), &|_| None, 0).0;
        assert_eq!(read("lib/a.rb"), file("lib/a.rb", b"def a; end\n"));
        assert_eq!(read("lib/big.rb"), file("lib/big.rb", large.as_bytes()));
        assert_eq!(read("lib/blob.rb"), file("lib/blob.rb", blob.as_bytes()));
        assert!(!unread(Lang::Ruby, "lib/a.rb", b"def a; end\n"));
        assert!(unread(Lang::Ruby, "lib/big.rb", large.as_bytes()));
        assert!(unread(Lang::Ruby, "lib/blob.rb", blob.as_bytes()));
        assert_eq!(read("bin/dev"), Read::Foreign);
        assert_eq!(
            read("lib/gone.rb"),
            Read::Skipped(Skipped {
                path: "lib/gone.rb".to_owned(),
                reason: SkipReason::Absent,
            })
        );
    }

    // D24-M1: a binstub of another language that is also a generated blob
    // was indexed as Ruby, unread.
    #[test]
    fn a_binstub_of_another_language_stays_out_however_large_or_generated() {
        let dir = tempfile::tempdir().unwrap();
        let node = format!("#!/usr/bin/env node\n{}\n", "var a=1;".repeat(12_000));
        let shell = format!("#!/bin/sh\n{}", "echo hi\n".repeat(140_000));
        let ruby = format!("#!/usr/bin/env ruby\n{}\n", "X = 1; ".repeat(12_000));
        let large = format!("#!/usr/bin/env ruby\n{}", "# padding\n".repeat(110_000));
        for (path, content) in [
            ("bin/node", &node),
            ("bin/shell", &shell),
            ("bin/ruby", &ruby),
            ("bin/large", &large),
        ] {
            std::fs::create_dir_all(dir.path().join("bin")).unwrap();
            std::fs::write(dir.path().join(path), content).unwrap();
        }
        let read = |path| read_one(dir.path(), &listed(path), &|_| None, 0).0;
        assert_eq!(read("bin/node"), Read::Foreign);
        assert_eq!(read("bin/shell"), Read::Foreign);
        assert_eq!(read("bin/ruby"), file("bin/ruby", ruby.as_bytes()));
        assert_eq!(read("bin/large"), file("bin/large", large.as_bytes()));
        assert!(unread(Lang::Ruby, "bin/ruby", ruby.as_bytes()));
        assert!(unread(Lang::Ruby, "bin/large", large.as_bytes()));
    }

    // D24-P1: a file past the cap was held whole in memory to be hashed.
    // IDX-R3-1: a large file of another language (a binary in `bin/`) was
    // then read and hashed whole on every scan, and left out.
    #[test]
    fn a_file_is_hashed_as_it_streams_by_and_one_of_another_language_read_to_its_head() {
        struct Counting<R> {
            inner: R,
            read: u64,
        }
        impl<R: std::io::Read> std::io::Read for Counting<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.read += n as u64;
                Ok(n)
            }
        }
        let body = || std::io::repeat(b'x').take(2 * SIZE_CAP);
        let mut node = Counting {
            inner: (&b"#!/usr/bin/env node\n"[..]).chain(body()),
            read: 0,
        };
        assert_eq!(
            hash_owned(Lang::Ruby, "bin/protoc", &mut node).unwrap(),
            None
        );
        assert!(
            node.read < GENERATED_MIN_BYTES as u64,
            "{} bytes read",
            node.read
        );
        let mut whole = b"#!/usr/bin/env ruby\n".to_vec();
        body().read_to_end(&mut whole).unwrap();
        let small: &[u8] = b"def a; end\n";
        for (path, content) in [("bin/large", &whole[..]), ("lib/a.rb", small)] {
            assert_eq!(
                hash_owned(Lang::Ruby, path, content).unwrap(),
                Some((Hash::of(content), content.len() as u64))
            );
        }
    }

    #[test]
    fn the_variables_cleared_are_the_ones_git_names_local() {
        let output = Command::new("git")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .unwrap();
        let listed = String::from_utf8(output.stdout).unwrap();
        let listed: Vec<&str> = listed.lines().collect();
        assert!(!listed.is_empty(), "git lists its local variables");
        for var in &listed {
            assert!(LOCAL_ENV.contains(var), "{var} is not cleared");
        }
    }

    /// Runs git in `dir`, as a test sets a repository up.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = git_in(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    // Review of #53, M3: a name with a space kept git's trailing TAB, a
    // C-quoted name was dropped, and `diff.noprefix` dropped every name.
    #[test]
    fn c9_the_diff_gives_each_changed_path_as_the_tree_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let names = [
            "app/pay ment.rb",
            "app/quo\"te.rb",
            "app/back\\slash.rb",
            "app/héllo wörld.rb",
            "app/tab\tname.rb",
            "app/plain.rb",
        ];
        run_git(root, &["init", "-q"]);
        for name in names {
            std::fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            std::fs::write(root.join(name), "a\nb\nc\n").unwrap();
        }
        std::fs::write(root.join("app/lines.rb"), "-- a/x\nkeep\n").unwrap();
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", "one"]);
        run_git(root, &["config", "diff.noprefix", "true"]);
        for name in names {
            std::fs::write(root.join(name), "a\nB\nc\n").unwrap();
        }
        // A removed line `-- a/x` and an added `++ b/fake.rb` print as
        // `--- a/x` and `+++ b/fake.rb` inside the hunk.
        std::fs::write(root.join("app/lines.rb"), "++ b/fake.rb\nkeep\n").unwrap();
        let diff = super::diff(root).unwrap();
        let mut expected: Vec<DiffFile> = names
            .iter()
            .map(|name| DiffFile {
                path: (*name).to_owned(),
                hunks: vec![(2, 2)],
            })
            .collect();
        expected.push(DiffFile {
            path: "app/lines.rb".to_owned(),
            hunks: vec![(1, 1)],
        });
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(diff.files, expected);
    }

    // The hook takes the chain's step 3 input from its scan (`changes`)
    // instead of listing the tree again (`diff`), 130 ms a prompt on a
    // 12k-file tree. Step 3 must see the same changes either way, in every
    // state a tree can be in, or the minimap moves with the shortcut.
    #[test]
    fn c9_the_changes_a_scan_gives_are_the_diff_in_every_tree_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |path: &str, content: &str| {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), content).unwrap();
        };
        let same = |state: &str| {
            let full = super::diff(root).unwrap();
            let fast = changes(&scan(root).unwrap()).unwrap();
            assert_eq!(fast.files, full.files, "{state}");
            full.files.len()
        };
        run_git(root, &["init", "-q"]);
        write("app/a.rb", "a\nb\n");
        write("app/pay ment.rb", "x\n");
        run_git(root, &["add", "app/a.rb"]);
        assert_eq!(
            same("before the first commit, one staged, one untracked"),
            2
        );
        run_git(root, &["add", "-A"]);
        write(".gitignore", "tmp/\n");
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", "one"]);
        assert_eq!(same("clean"), 0);
        write("tmp/ignored.rb", "x\n");
        write(".pixel-mvp/graph.db", "state\n");
        assert_eq!(same("only ignored files and Pixel's state"), 0);
        write("app/pay ment.rb", "y\n");
        assert_eq!(same("a tracked file edited"), 1);
        write("notes.txt", "n\n");
        write("lib/new.rb", "n\n");
        assert_eq!(same("untracked files in and out of the language set"), 3);
        write("app/a.rb", "a\nB\n");
        run_git(root, &["add", "app/a.rb"]);
        assert_eq!(same("a staged edit"), 4);
        std::fs::remove_file(root.join("app/pay ment.rb")).unwrap();
        same("a tracked file removed");
    }

    // ADR-0022 §2: the scan takes a file's hash from the index while its
    // stat holds. The risk is an old hash served for new content; a write
    // that keeps the size and sets the old mtime back still moves ctime.
    #[test]
    fn c9_a_hash_is_reused_only_while_the_stat_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        std::fs::write(root.join("a.rb"), "def a; end\n").unwrap();
        let meta = std::fs::symlink_metadata(root.join("a.rb")).unwrap();
        let recorded = file_stat(&meta).unwrap();
        let stand_in = Hash::of(b"what the index recorded");
        let known = |path: &str| (path == "a.rb").then_some((stand_in, recorded));
        let hash_of = |tree: Tree| tree.files.iter().find(|f| f.path == "a.rb").map(|f| f.hash);
        assert_eq!(
            hash_of(scan_with(root, &known).unwrap()),
            Some(stand_in),
            "stat unchanged: not read"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("a.rb"))
            .unwrap();
        std::io::Write::write_all(&mut &file, b"def b; end\n").unwrap();
        file.set_modified(meta.modified().unwrap()).unwrap();
        drop(file);
        assert_eq!(
            hash_of(scan_with(root, &known).unwrap()),
            Some(Hash::of(b"def b; end\n")),
            "same size, mtime set back: ctime moved, so the file is read"
        );
    }

    // git's racy-clean rule: a write later in the second a stat was taken
    // can leave a coarse clock's times as they were, so a stat from the
    // scan's own second is never kept, by its mtime or by its ctime.
    #[test]
    fn a_stat_from_the_scans_own_second_is_not_kept() {
        let stat = |mtime: i64, ctime: i64| FileStat {
            len: 1,
            mtime_ns: mtime * 1_000_000_000 + 5,
            ctime_ns: ctime * 1_000_000_000 + 5,
            ino: 1,
        };
        assert_eq!(settled(Some(stat(9, 9)), 10), Some(stat(9, 9)));
        assert_eq!(settled(Some(stat(10, 9)), 10), None);
        assert_eq!(settled(Some(stat(9, 10)), 10), None);
        assert_eq!(settled(Some(stat(11, 11)), 10), None);
        assert_eq!(settled(None, 10), None);
    }

    #[test]
    fn a_quoted_name_reads_back_byte_for_byte() {
        assert_eq!(
            new_path("\"b/a\\\"b\\\\c\\303\\251\\td.rb\""),
            Some("a\"b\\cé\td.rb".to_owned())
        );
        assert_eq!(new_path("b/pay ment.rb\t"), Some("pay ment.rb".to_owned()));
        assert_eq!(new_path("/dev/null"), None);
        assert_eq!(new_path("\"b/\\377.rb\""), None, "not UTF-8");
        assert_eq!(lengths("-3,0 +4,2 @@ fn x"), Some((0, 2)));
        assert_eq!(lengths("-3 +4 @@"), Some((1, 1)));
    }
}
