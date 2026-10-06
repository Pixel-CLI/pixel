// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Exact, deliberately narrow native-search compatibility behind
//! `pixel search-like-rg`, which a user or a shell alias points at in place of
//! `rg` or `grep`.
//! Unsupported inputs execute the original search; ordinary `pixel search`
//! keeps its richer, bounded interface. Never emit a partial native result.
//!
//! The contract is the native tool's bytes and exit status, with one
//! deliberate divergence: `rg <pattern>` with no path. Native `rg` searches
//! stdin when stdin is a pipe or a regular file, even an empty one (an
//! empty pipe gives no match and exit 1), and the current directory when
//! stdin is a terminal or another device such as `/dev/null`. An agent's
//! shell tool runs commands with a pipe on stdin that nothing writes to, so
//! native `rg` blocks there until the call times out. The emulation never
//! reads stdin: it always answers the current-directory search, which is
//! what `rg` does with `/dev/null` on stdin. When the emulation falls back,
//! [`run`] executes the original `rg` with the inherited stdin, native
//! behaviour included.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SearchTool {
    Rg,
    Grep,
}

impl SearchTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rg => "rg",
            Self::Grep => "grep",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SearchArgs {
    pattern: String,
    /// `None` means the implicit current-directory search (`rg foo`,
    /// `grep -r foo`). Bare `grep foo` reads stdin and never reaches this.
    path: Option<String>,
    recursive: bool,
    numbered: bool,
    filename: bool,
    no_filename: bool,
}

/// Parse only a literal search over a single explicit file, a single
/// directory, or the implicit cwd. Every unknown option falls back rather
/// than being dropped. Last -H/-h wins, like the tools.
pub fn parse_args(tool: SearchTool, args: &[String]) -> Option<SearchArgs> {
    let mut numbered = false;
    let mut filename = false;
    let mut no_filename = false;
    let mut recursive = false;
    let mut fixed = false;
    let mut positional = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            if !positional.is_empty() {
                return None;
            }
            options = false;
            continue;
        }
        if options && arg.starts_with('-') {
            if !positional.is_empty() {
                // GNU option permutation and BSD/POSIX argument ordering
                // differ. Never reinterpret a flag after the pattern.
                return None;
            }
            match arg.as_str() {
                "--line-number" => numbered = true,
                "--with-filename" => {
                    filename = true;
                    no_filename = false;
                }
                "--no-filename" => {
                    filename = false;
                    no_filename = true;
                }
                "--recursive" if tool == SearchTool::Grep => recursive = true,
                "--fixed-strings" => fixed = true,
                _ if arg.len() > 1 && !arg.starts_with("--") => {
                    for flag in arg[1..].chars() {
                        match flag {
                            'n' => numbered = true,
                            'H' => {
                                filename = true;
                                no_filename = false;
                            }
                            'h' if tool == SearchTool::Grep => {
                                filename = false;
                                no_filename = true;
                            }
                            'r' if tool == SearchTool::Grep => recursive = true,
                            'F' => fixed = true,
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            }
        } else {
            positional.push(arg);
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        return None;
    }
    let pattern = positional[0];
    if pattern.is_empty()
        || !pattern.is_ascii()
        || pattern.bytes().any(|b| b.is_ascii_control())
        || (!fixed
            && !pattern
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_ -".contains(&b)))
    {
        return None;
    }
    let path = positional.get(1).map(|p| (*p).clone());
    match &path {
        // "-" and "" name stdin or nothing searchable; a missing path is
        // stdin for bare grep, an implicit cwd search for `rg`/`grep -r`.
        Some(p) if *p == "-" || p.is_empty() => None,
        None if tool == SearchTool::Grep && !recursive => None,
        _ => Some(SearchArgs {
            pattern: pattern.clone(),
            path: path.clone(),
            recursive,
            numbered,
            filename,
            no_filename,
        }),
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn checked_path(raw: &str, cwd: &Path) -> Option<(PathBuf, PathBuf, String)> {
    if credential_path(Path::new(raw)) {
        return None;
    }
    let path = cwd.join(raw);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > pixel_index::index::MAX_FILE_BYTES {
        return None;
    }
    let abs = path.canonicalize().ok()?;
    let root = crate::discover_root(cwd).ok()?;
    if !root.join(".pixel").is_dir() {
        return None;
    }
    let relative = abs.strip_prefix(&root).ok()?.to_str()?.to_string();
    if relative.starts_with(".pixel/") || relative.starts_with(".git/") || credential_path(&abs) {
        return None;
    }
    Some((root, abs, relative))
}

/// A directory (or the implicit cwd) qualifies only inside an indexed repo.
/// Per-file eligibility is not decidable here without walking the tree, so
/// the execution side re-verifies every file the native tool would open and
/// falls back on any gap — see `dir_output`.
fn checked_dir(raw: &str, cwd: &Path) -> Option<(PathBuf, PathBuf, String)> {
    if credential_path(Path::new(raw)) {
        return None;
    }
    let dir = cwd.join(raw);
    let metadata = std::fs::symlink_metadata(&dir).ok()?;
    if !metadata.file_type().is_dir() {
        return None;
    }
    let abs = dir.canonicalize().ok()?;
    let root = crate::discover_root(cwd).ok()?;
    if !root.join(".pixel").is_dir() {
        return None;
    }
    let relative = abs.strip_prefix(&root).ok()?.to_str()?.to_string();
    if relative == ".pixel"
        || relative == ".git"
        || relative.starts_with(".pixel/")
        || relative.starts_with(".git/")
        || credential_path(&abs)
    {
        return None;
    }
    Some((root, abs, relative))
}

/// Credential-shaped paths never qualify for automatic permission grants.
/// This is metadata-only eligibility, not a claim to detect every secret.
/// The single canonical predicate lives in `pixel_index::index::credential_path`
/// (shared with the daemon's search filter; the task sandbox keeps its own).
fn credential_path(path: &Path) -> bool {
    pixel_index::index::credential_path(path)
}

/// The user configured the tool being replaced: its native output may no
/// longer match the literal-search emulation, so the command stays native.
/// Only that tool's own configuration counts. `RIPGREP_CONFIG_PATH` changes
/// nothing about `grep` and `GREP_OPTIONS` nothing about `rg`; a developer
/// with an rg config would otherwise never get a `grep` emulation.
pub(crate) fn native_configuration(tool: SearchTool) -> bool {
    let variable = match tool {
        SearchTool::Rg => "RIPGREP_CONFIG_PATH",
        SearchTool::Grep => "GREP_OPTIONS",
    };
    std::env::var_os(variable).is_some()
}

fn compatible_output(
    tool: SearchTool,
    args: &SearchArgs,
    cwd: &Path,
) -> Result<(Vec<u8>, i32, &'static str), &'static str> {
    if std::io::stdout().is_terminal() || native_configuration(tool) {
        return Err("native-configuration");
    }
    if let Some(raw) = args.path.as_deref()
        && let Some((root, abs, relative)) = checked_path(raw, cwd)
    {
        return file_output(tool, args, &root, &abs, &relative, raw);
    }
    dir_output(tool, args, cwd)
}

fn file_output(
    tool: SearchTool,
    args: &SearchArgs,
    root: &Path,
    abs: &Path,
    relative: &str,
    raw: &str,
) -> Result<(Vec<u8>, i32, &'static str), &'static str> {
    let before = pixel_index::index::read_regular_bounded(abs, pixel_index::index::MAX_FILE_BYTES)
        .map_err(|_| "unreadable-file")?;
    if !before.is_ascii() || before.contains(&0) || before.contains(&b'\r') {
        return Err("unsupported-file-bytes");
    }
    let mut index = pixel_index::indexset::IndexSet::open_or_build(
        root,
        Box::new(pixel_index::TrigramExtractor),
    )
    .map_err(|_| "index-unavailable")?;
    // Refresh the exact file synchronously: a stale watcher must never turn
    // a newly added match into a false negative in the compatibility path.
    index.refresh_file(relative);
    if !index.paths().contains(&relative.to_string()) {
        return Err("file-not-covered");
    }
    let (matches, stats) = index
        .search_page_in(
            &regex::escape(&args.pattern),
            0,
            Some(10_000),
            Some(&[relative.to_string()]),
        )
        .map_err(|_| "search-failed")?;
    if stats.truncated {
        return Err("result-cap");
    }
    if pixel_index::index::read_regular_bounded(abs, pixel_index::index::MAX_FILE_BYTES)
        .ok()
        .as_deref()
        != Some(before.as_slice())
    {
        return Err("file-changed");
    }
    // `grep -r` names even a single explicit file unless -h; `rg` keeps its
    // single-file default of no filename.
    let show_name =
        args.filename || (tool == SearchTool::Grep && args.recursive && !args.no_filename);
    let mut output = Vec::new();
    for m in &matches {
        if show_name {
            output.extend_from_slice(raw.as_bytes());
            output.push(b':');
        }
        if args.numbered {
            output.extend_from_slice(m.line_number.to_string().as_bytes());
            output.push(b':');
        }
        output.extend_from_slice(m.line.as_bytes());
        output.push(b'\n');
        if output.len() > 64 * 1024 {
            return Err("output-cap");
        }
    }
    Ok((
        output,
        i32::from(matches.is_empty()),
        "equivalent-literal-file",
    ))
}

/// Absolute paths of every file the native tool would open under `dir`.
/// `rg` skips hidden entries and honors ignore files (the `ignore` walk is
/// its own crate, so the filters agree by construction); `grep -r` opens
/// everything recursively and follows no symlink below the argument.
/// Most files a directory emulation reads, refreshes and re-stats. Past it
/// the native tool is faster than proving every file current, and the
/// walk must stay cheap.
const DIR_EMULATION_MAX_FILES: usize = 2_000;

/// Most bytes a directory emulation reads before handing the search back.
const DIR_EMULATION_MAX_BYTES: u64 = 33_554_432; // 32 MiB

/// Every file the native tool would open under `dir`, or an error as soon
/// as there are more than `max_files` of them.
fn dir_entries(
    tool: SearchTool,
    dir: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, &'static str> {
    let mut paths = Vec::new();
    match tool {
        SearchTool::Rg => {
            for entry in ignore::WalkBuilder::new(dir).build() {
                let entry = entry.map_err(|_| "walk-failed")?;
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    push_bounded(&mut paths, entry.into_path(), max_files)?;
                }
            }
        }
        SearchTool::Grep => grep_walk(dir, &mut paths, max_files)?,
    }
    Ok(paths)
}

fn push_bounded(
    out: &mut Vec<PathBuf>,
    path: PathBuf,
    max_files: usize,
) -> Result<(), &'static str> {
    if out.len() >= max_files {
        return Err("tree-too-large");
    }
    out.push(path);
    Ok(())
}

fn grep_walk(dir: &Path, out: &mut Vec<PathBuf>, max_files: usize) -> Result<(), &'static str> {
    for entry in std::fs::read_dir(dir).map_err(|_| "walk-failed")? {
        let entry = entry.map_err(|_| "walk-failed")?;
        let kind = entry.file_type().map_err(|_| "walk-failed")?;
        if kind.is_dir() && entry.file_name() == ".git" {
            // `grep -r` reads git's object store too, whose compressed
            // objects always hold NUL bytes: hand the search back now
            // instead of after reading the whole store.
            return Err("vcs-dir-in-tree");
        } else if kind.is_dir() {
            grep_walk(&entry.path(), out, max_files)?;
        } else if kind.is_file() {
            push_bounded(out, entry.path(), max_files)?;
        } else {
            // Symlinks, fifos, sockets: `grep -r` opens them for real; the
            // emulation has no answer, so the command stays native.
            return Err("non-regular-entry");
        }
    }
    Ok(())
}

/// Directory emulation is all-or-nothing per file: every file the native
/// tool would open must be index-admitted, regular, bounded, NUL-free, and
/// unchanged through the search, or the original command executes instead.
/// Residual coverage gap versus native: a file whose only match hides
/// behind bytes the verifier cannot read is impossible here (NUL anywhere
/// disqualifies the whole search up front), but a file that appears between
/// the eligibility walk and `refresh_files` can still slip to native
/// ordering — the post-search stat check catches it instead.
fn dir_output(
    tool: SearchTool,
    args: &SearchArgs,
    cwd: &Path,
) -> Result<(Vec<u8>, i32, &'static str), &'static str> {
    if tool == SearchTool::Grep && !args.recursive {
        return Err("directory-without-recursive");
    }
    let raw = args.path.as_deref();
    let (root, dir, dir_rel) = match raw {
        Some(raw) => checked_dir(raw, cwd),
        // A no-operand `grep -r` prints platform-specific names: `./a` on BSD,
        // `a` on GNU, so no single emulation matches it.
        None if tool == SearchTool::Grep => return Err("implicit-grep-cwd"),
        None => checked_dir(
            cwd.canonicalize()
                .ok()
                .and_then(|abs| abs.to_str().map(str::to_string))
                .as_deref()
                .ok_or("bad-cwd")?,
            cwd,
        ),
    }
    .ok_or("unsupported-directory")?;
    let mut files = Vec::new();
    let mut clean = HashMap::new();
    let mut read_bytes: u64 = 0;
    for path in dir_entries(tool, &dir, DIR_EMULATION_MAX_FILES)? {
        let rel = path
            .strip_prefix(&root)
            .ok()
            .and_then(|p| p.to_str())
            .ok_or("outside-root")?
            .to_string();
        // One credential-shaped entry under the tree keeps the whole search
        // native, preserving the permission boundary of the original path.
        if credential_path(Path::new(&rel)) {
            return Err("credential-in-tree");
        }
        let bytes =
            pixel_index::index::read_regular_bounded(&path, pixel_index::index::MAX_FILE_BYTES)
                .map_err(|_| "unreadable-file")?;
        read_bytes += bytes.len() as u64;
        if read_bytes > DIR_EMULATION_MAX_BYTES {
            return Err("tree-too-large");
        }
        // NUL anywhere can hide a post-quit match or a "binary file matches"
        // notice the emulation cannot reproduce — strict fail-open.
        if bytes.contains(&0) {
            return Err("binary-file");
        }
        // Non-ASCII or CR bytes only corrupt emulation when the file actually
        // matches, so they are tolerated here and rejected per match below.
        clean.insert(rel.clone(), bytes.is_ascii() && !bytes.contains(&b'\r'));
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| "walk-failed")?;
        files.push((rel, metadata.len(), metadata.modified().ok()));
    }
    let mut index = pixel_index::indexset::IndexSet::open_or_build(
        &root,
        Box::new(pixel_index::TrigramExtractor),
    )
    .map_err(|_| "index-unavailable")?;
    // Refresh every eligible file synchronously: this both picks up files
    // created since the last watcher event and proves each one is admitted
    // by the indexing policy — an Excluded outcome means the index can never
    // see the file, so the search would under-report versus native.
    for (_, outcome) in index.refresh_files(
        &files
            .iter()
            .map(|(rel, ..)| (rel.as_str(), false))
            .collect::<Vec<_>>(),
    ) {
        if outcome != pixel_index::indexset::RefreshOutcome::Indexed {
            return Err("file-not-covered");
        }
    }
    let (matches, stats) = index
        .search_page_in(
            &regex::escape(&args.pattern),
            0,
            Some(10_000),
            Some(std::slice::from_ref(&dir_rel)),
        )
        .map_err(|_| "search-failed")?;
    if stats.truncated {
        return Err("result-cap");
    }
    for (rel, len, modified) in &files {
        let metadata = std::fs::symlink_metadata(root.join(rel)).map_err(|_| "file-changed")?;
        if !metadata.file_type().is_file()
            || metadata.len() != *len
            || metadata.modified().ok() != *modified
        {
            return Err("file-changed");
        }
    }
    // rg always names files in a directory search; `grep -r` unless -h.
    let show_name = match tool {
        SearchTool::Rg => true,
        SearchTool::Grep => !args.no_filename,
    };
    let mut output = Vec::new();
    let mut printed = 0usize;
    for m in &matches {
        // Matches can also come from indexed files the tool would not open
        // (hidden, ignored, policy-pruned): the native search skips them,
        // so drop rather than fail. An eligible match on unclean bytes is
        // the actual emulation failure.
        match clean.get(m.path.as_str()) {
            None => continue,
            Some(false) => return Err("unsupported-file-bytes"),
            Some(true) => {}
        }
        let sub = Path::new(&m.path)
            .strip_prefix(&dir_rel)
            .map_err(|_| "outside-root")?;
        let display = match raw {
            Some(raw) => format!("{}/{}", raw.trim_end_matches('/'), sub.display()),
            // Only `rg` reaches an implicit cwd search: bare relative paths.
            None => sub.display().to_string(),
        };
        printed += 1;
        if show_name {
            output.extend_from_slice(display.as_bytes());
            output.push(b':');
        }
        if args.numbered {
            output.extend_from_slice(m.line_number.to_string().as_bytes());
            output.push(b':');
        }
        output.extend_from_slice(m.line.as_bytes());
        output.push(b'\n');
        if output.len() > 64 * 1024 {
            return Err("output-cap");
        }
    }
    // The status follows what was printed: matches dropped above are
    // files the native tool never opens, so they cannot make it exit 0.
    Ok((output, i32::from(printed == 0), "equivalent-literal-dir"))
}

fn record(cwd: &Path, backend: &str, reason: &str) {
    if let Ok(root) = crate::discover_root(cwd) {
        let mut logger = pixel_actionlog::ActionLog::spawn_for_root(&root);
        logger.log(pixel_actionlog::ActionEvent::new(
            "search-compat",
            format!("backend={backend} reason={reason}"),
        ));
        // `run` exits right after: only a flushed record is observable.
        logger.finish_flush();
    }
}

pub fn run(tool: SearchTool, argv: Vec<String>) -> ! {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let result = parse_args(tool, &argv)
        .ok_or("unsupported-arguments")
        .and_then(|args| compatible_output(tool, &args, &cwd));
    match result {
        Ok((output, code, reason)) => {
            record(&cwd, "pixel", reason);
            if let Err(error) = std::io::stdout().write_all(&output) {
                eprintln!("{}: {error}", tool.name());
                std::process::exit(2);
            }
            std::process::exit(code);
        }
        Err(reason) => {
            if std::env::var_os("PIXEL_COMPAT_DEBUG").is_some() {
                eprintln!("compat-fallback: {reason}");
            }
            // Do not write observability files before a native fallback:
            // recursive searches may include the log in their own corpus.
            let error = std::process::Command::new(tool.name()).args(&argv).exec();
            eprintln!("{}: {error}", tool.name());
            std::process::exit(127);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(ToString::to_string).collect()
    }

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "pixel-search-compat-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join(".pixel")).unwrap();
            std::fs::create_dir_all(root.join("src/sub")).unwrap();
            std::fs::write(root.join("src/a.rs"), b"needle one\nplain\n").unwrap();
            std::fs::write(root.join("src/sub/b.rs"), b"needle two\n").unwrap();
            Self(root.canonicalize().unwrap())
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn literal_parser_preserves_flags_and_rejects_unknown_semantics() {
        let parsed = parse_args(SearchTool::Grep, &args(&["-nHFh", "foo.bar", "a.rs"])).unwrap();
        assert_eq!(parsed.pattern, "foo.bar");
        assert!(parsed.numbered);
        assert!(!parsed.filename);
        for raw in [
            vec!["-rln", "needle", "."],
            vec!["-A20", "needle", "a.rs"],
            vec!["-i", "needle", "a.rs"],
            vec!["needle", "a.rs", "b.rs"],
            vec!["foo.*", "a.rs"],
            vec!["", "a.rs"],
            vec!["needle", "-n", "a.rs"],
            vec!["needle", "-"],
            vec!["needle", ""],
        ] {
            assert!(
                parse_args(SearchTool::Grep, &args(&raw)).is_none(),
                "{raw:?}"
            );
        }
        assert!(parse_args(SearchTool::Rg, &args(&["-h", "needle", "a.rs"])).is_none());
        // Bare grep with no path reads stdin; `rg`/`grep -r` mean the cwd.
        assert!(parse_args(SearchTool::Grep, &args(&["needle"])).is_none());
        assert!(parse_args(SearchTool::Rg, &args(&["needle"])).is_some());
        assert!(parse_args(SearchTool::Grep, &args(&["-rn", "needle"])).is_some());
        // `rg` has no -r: it is --replace and must stay native.
        assert!(parse_args(SearchTool::Rg, &args(&["-rn", "needle", "a"])).is_none());
    }

    #[test]
    fn long_flags_and_a_lone_dash_parse_as_their_tool_allows() {
        let grep_r = parse_args(SearchTool::Grep, &args(&["--recursive", "needle"])).unwrap();
        assert!(grep_r.recursive);
        assert!(parse_args(SearchTool::Rg, &args(&["--recursive", "needle", "a"])).is_none());
        let fixed = parse_args(
            SearchTool::Grep,
            &args(&["--fixed-strings", "foo.bar", "a.rs"]),
        )
        .unwrap();
        assert_eq!(fixed.pattern, "foo.bar");
        assert!(parse_args(SearchTool::Grep, &args(&["-", "needle", "a.rs"])).is_none());
        assert!(parse_args(SearchTool::Rg, &args(&["-n"])).is_none());
    }

    #[test]
    fn checked_dir_refuses_the_index_and_git_directories() {
        let repo = Repo::new();
        for dir in [".git/objects", ".pixel/shard"] {
            std::fs::create_dir_all(repo.0.join(dir)).unwrap();
        }
        for raw in [".git", ".pixel", ".git/objects", ".pixel/shard"] {
            assert!(checked_dir(raw, &repo.0).is_none(), "{raw}");
        }
        let (root, abs, relative) = checked_dir("src", &repo.0).unwrap();
        assert_eq!(
            (root, abs, relative),
            (repo.0.clone(), repo.0.join("src"), "src".to_string())
        );
    }

    #[test]
    fn a_directory_search_past_the_byte_budget_hands_the_search_back() {
        let repo = Repo::new();
        let big = repo.0.join("src/big");
        std::fs::create_dir_all(&big).unwrap();
        let line = "filler line without the word\n".repeat(130_000); // ~3.8 MB
        for n in 0..9 {
            std::fs::write(big.join(format!("f{n}.txt")), &line).unwrap();
        }
        let parsed = parse_args(SearchTool::Grep, &args(&["-r", "needle", "src"])).unwrap();
        assert_eq!(
            dir_output(SearchTool::Grep, &parsed, &repo.0).err(),
            Some("tree-too-large")
        );
        let plain = parse_args(SearchTool::Grep, &args(&["needle", "src"])).unwrap();
        assert_eq!(
            dir_output(SearchTool::Grep, &plain, &repo.0).err(),
            Some("directory-without-recursive")
        );
    }

    #[test]
    fn a_walk_past_the_file_budget_hands_the_search_back() {
        let repo = Repo::new();
        let src = repo.0.join("src");
        // Two files under src: a budget of two fits, one does not.
        for tool in [SearchTool::Rg, SearchTool::Grep] {
            assert_eq!(dir_entries(tool, &src, 2).map(|p| p.len()), Ok(2));
            assert_eq!(dir_entries(tool, &src, 1), Err("tree-too-large"));
        }
    }

    #[test]
    fn a_match_only_in_a_file_rg_skips_prints_nothing_and_exits_one() {
        let repo = Repo::new();
        std::fs::create_dir_all(repo.0.join(".hidden")).unwrap();
        std::fs::write(repo.0.join(".hidden/x.rs"), b"lonely_marker\n").unwrap();
        let lonely = parse_args(SearchTool::Rg, &args(&["lonely_marker"])).unwrap();
        let (output, status, _) = dir_output(SearchTool::Rg, &lonely, &repo.0).unwrap();
        assert_eq!(
            (String::from_utf8_lossy(&output).into_owned(), status),
            (String::new(), 1),
            "native rg skips `.hidden/`, so it finds nothing and exits 1"
        );
        let found = parse_args(SearchTool::Rg, &args(&["needle"])).unwrap();
        let (output, status, _) = dir_output(SearchTool::Rg, &found, &repo.0).unwrap();
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&output));
    }

    #[test]
    fn grep_r_over_a_git_directory_goes_native_before_reading_it() {
        let repo = Repo::new();
        std::fs::create_dir_all(repo.0.join(".git/objects")).unwrap();
        std::fs::write(repo.0.join(".git/config"), b"needle\n").unwrap();
        assert_eq!(
            dir_entries(SearchTool::Grep, &repo.0, DIR_EMULATION_MAX_FILES),
            Err("vcs-dir-in-tree")
        );
        // rg skips hidden directories natively, so its walk is unaffected.
        assert!(dir_entries(SearchTool::Rg, &repo.0, DIR_EMULATION_MAX_FILES).is_ok());
    }
}
