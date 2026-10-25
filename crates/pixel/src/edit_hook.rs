// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel run-hook edit-event`: the optional edit hooks, ported from RTO's
//! `pre-edit` and `post-edit`.
//!
//! Before an agent's edit tool writes a file, the card names the callers a
//! changed or removed declaration would strand; after it, the callers that
//! still sit on the old declaration. Both are off unless `edit_hooks` is on
//! (`pixel config edit_hooks on`, then `pixel setup`), and both fail open: a
//! payload that is not an edit, a stale or missing graph, a switch that is off
//! or a read that takes longer than [`DEADLINE`] prints nothing and exits 0.
//! The graph is read as it stands (`impact_read::open_fresh`): no index is
//! built or repaired here, and the callers are the graph's `Calls` edges, so
//! they are a lower bound, as everywhere in Pixel.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use pixel_graph::{EdgeKind, GraphStore, SymbolRow, Tier};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::brief_parts::{hex, refuse_linked_dirs};
use crate::task_hook::TaskProvider;

/// How long a hook may hold the edit: a late card is dropped, not waited for.
const DEADLINE: Duration = Duration::from_millis(1000);
/// The most a card or a stale note says, in bytes.
const TEXT_MAX_BYTES: usize = 1200;
/// Callers a card lists as certain, and as only name matches.
const MAX_PROVED: usize = 8;
const MAX_POSSIBLE: usize = 4;
/// Changed declarations a session keeps waiting for their callers.
const MAX_PENDING: usize = 16;
/// Where a session's state lives under `.pixel/`.
const SESSION_DIR: &str = "edit-hooks";
/// A session file older than this is deleted by the next write.
const SESSION_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest session file read back; a bigger one counts as absent.
const SESSION_MAX_BYTES: u64 = 256 * 1024;
/// The largest source file an edit is checked against.
const SOURCE_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditEvent {
    PreEdit,
    PostEdit,
}

impl EditEvent {
    /// The host event the hook rides.
    const fn host_name(self) -> &'static str {
        match self {
            Self::PreEdit => "PreToolUse",
            Self::PostEdit => "PostToolUse",
        }
    }
}

/// A call site of a declaration an edit changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Site {
    file: String,
    line: u32,
    holder: String,
    /// The caller file's content hash when the card named it; empty for a
    /// site that is only a name match.
    hash: String,
}

/// A declaration whose callers in other files were named by a card and may
/// still be on the old signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    name: String,
    kind: String,
    file: String,
    old: String,
    announced: bool,
    sites: Vec<Site>,
}

/// What the hooks remember between two events of one agent session.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    /// `<uid>|<change>` of every card already sent: once per change.
    carded: BTreeSet<String>,
    pending: Vec<Pending>,
}

impl Session {
    fn path(root: &Path, session: &str) -> PathBuf {
        root.join(pixel_index::index::SHARD_DIR)
            .join(SESSION_DIR)
            .join(format!("{}.json", &hex(session)[..16]))
    }

    fn load(path: &Path) -> Self {
        let read = || -> Option<Self> {
            refuse_linked_dirs(path).ok()?;
            let file = pixel_git::nofollow::open_read(path).ok()?;
            let mut text = String::new();
            file.take(SESSION_MAX_BYTES + 1)
                .read_to_string(&mut text)
                .ok()?;
            (text.len() as u64 <= SESSION_MAX_BYTES)
                .then(|| serde_json::from_str(&text).ok())
                .flatten()
        };
        read().unwrap_or_default()
    }

    /// Best effort: a session that cannot be saved just forgets.
    fn save(&self, path: &Path) {
        let _ = refuse_linked_dirs(path).and_then(|()| {
            let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
            std::fs::create_dir_all(dir)?;
            crate::brief_parts::prune(dir, SystemTime::now(), SESSION_MAX_AGE);
            let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
            pixel_git::nofollow::write_replace(path, &bytes, pixel_git::nofollow::PRIVATE_MODE)
        });
    }
}

/// How a declaration changed.
enum Change {
    Reshaped(String),
    Removed,
}

/// A call of the changed declaration from outside it.
struct Caller {
    file: String,
    line: u32,
    holder: String,
    proved: bool,
}

pub fn run(provider: TaskProvider, event: EditEvent) -> ! {
    crate::hook_input::exit_0_after(|| respond(provider, event))
}

fn respond(provider: TaskProvider, event: EditEvent) {
    if !matches!(
        provider,
        TaskProvider::Claude | TaskProvider::Codex | TaskProvider::Devin
    ) {
        return;
    }
    let Some(raw) = crate::hook_input::read_hook_payload() else {
        return;
    };
    crate::hook_input::test_panic("edit-event");
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("pixel-hook-edit".into())
        .spawn(move || {
            let _ = sender.send(answer(event, &payload));
        });
    if spawned.is_err() {
        return;
    }
    let Ok(Some(text)) = receiver.recv_timeout(DEADLINE) else {
        return;
    };
    let output = json!({"hookSpecificOutput": {
        "hookEventName": event.host_name(), "additionalContext": text
    }});
    println!("{output}");
}

/// The text to inject for the edit `payload` describes, or `None`.
fn answer(event: EditEvent, payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name")?.as_str()?;
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let root = crate::discover_root(&cwd).ok()?.canonicalize().ok()?;
    if !crate::config_cmd::edit_hooks_on(Some(&root)) {
        return None;
    }
    let session_id = payload.get("session_id")?.as_str()?;
    let read = |path: &str| -> Option<String> {
        let file = repo_relative(&root, &cwd, path)?;
        read_source(&root, &file)
    };
    let targets = match event {
        EditEvent::PreEdit => {
            pixel_setup::edit_input::targets(tool, payload.get("tool_input")?, &read)
        }
        EditEvent::PostEdit => {
            pixel_setup::edit_input::targets(tool, payload.get("tool_input")?, &|_| None)
        }
    };
    let targets: Vec<(String, Option<String>)> = targets
        .into_iter()
        .filter_map(|target| Some((repo_relative(&root, &cwd, &target.path)?, target.proposed)))
        .collect();
    if targets.is_empty() {
        return None;
    }
    let path = Session::path(&root, session_id);
    let mut session = Session::load(&path);
    let texts: Vec<String> = match event {
        EditEvent::PreEdit => {
            let store = crate::impact_read::open_fresh(&root).ok()?;
            targets
                .iter()
                .filter_map(|(file, proposed)| {
                    card(&store, &mut session, &root, file, proposed.as_deref()?)
                })
                .collect()
        }
        EditEvent::PostEdit => targets
            .iter()
            .filter_map(|(file, _)| stale(&mut session, &root, file))
            .collect(),
    };
    session.save(&path);
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

/// `path` (absolute, or relative to `cwd`) as a repository-relative path with
/// `/` separators; `None` outside the repository, inside `.git`, or behind a
/// symbolic link or a directory.
fn repo_relative(root: &Path, cwd: &Path, path: &str) -> Option<String> {
    let full = cwd.join(path);
    let parent = full.parent()?.canonicalize().ok()?;
    let joined = parent.join(full.file_name()?);
    if std::fs::symlink_metadata(&joined)
        .is_ok_and(|meta| meta.file_type().is_symlink() || meta.is_dir())
    {
        return None;
    }
    let relative = joined.strip_prefix(root).ok()?;
    let parts: Vec<&str> = relative
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty() && parts[0] != ".git").then(|| parts.join("/"))
}

fn read_source(root: &Path, file: &str) -> Option<String> {
    let path = root.join(file);
    if std::fs::metadata(&path).ok()?.len() > SOURCE_MAX_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// The declaration header of an extracted signature, which is the first line
/// of the declaration and so may hold the body of a one-liner: cut at the
/// first `{` or `=>` outside brackets (a Python `def` at its `:`), with the
/// whitespace folded, so a reflow or a body edit is not a change.
///
/// shortcut: a `{` inside a generic argument ends the header early, which
/// hides a later change of that header; upgrade by asking the graph for a
/// parsed header (RTO's `shape`) if a language needs it.
fn shape(sig: &str) -> String {
    let python = sig.starts_with("def ") || sig.starts_with("async def ");
    let bytes = sig.as_bytes();
    let mut depth = 0_i32;
    let mut end = sig.len();
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' if depth == 0 => end = at,
            b'=' if depth == 0 && bytes.get(at + 1) == Some(&b'>') => end = at,
            b':' if depth == 0 && python => end = at,
            _ => continue,
        }
        if end == at {
            break;
        }
    }
    sig[..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .trim_end()
        .to_owned()
}

/// The hash of `file`'s content now, empty when it cannot be read.
fn hash_of(root: &Path, file: &str) -> String {
    read_source(root, file).map_or_else(String::new, |text| hex(&text))
}

/// Every call of `symbol` from outside the declaration itself.
fn callers(store: &GraphStore, symbol: &SymbolRow, file: &str) -> Result<Vec<Caller>, String> {
    let mut found = Vec::new();
    for edge in store
        .edges_to(symbol.id, Some(EdgeKind::Calls))
        .map_err(|e| e.to_string())?
    {
        let (path, holder): (String, String) = store
            .conn()
            .query_row(
                "SELECT f.path, s.qualified FROM symbols s JOIN files f ON f.id = s.file_id \
                 WHERE s.id = ?1",
                [edge.src_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let inside =
            path == file && (symbol.start_line..=symbol.end_line).contains(&edge.site_line);
        if !inside {
            found.push(Caller {
                file: path,
                line: edge.site_line,
                holder,
                proved: edge.tier == Tier::Exact,
            });
        }
    }
    found.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    found.dedup_by(|a, b| (&a.file, a.line) == (&b.file, b.line));
    Ok(found)
}

/// Appends `line` to `out` while the text stays within [`TEXT_MAX_BYTES`].
fn push_bounded(out: &mut String, line: &str) -> bool {
    if out.len() + line.len() + 1 > TEXT_MAX_BYTES {
        return false;
    }
    out.push_str(line);
    out.push('\n');
    true
}

/// The `[PIXEL:EDIT]` card for the declarations of `file` that `proposed`
/// reshapes or removes while others still call them. Remembers what it sent
/// so the same change is carded once, and the callers in other files for
/// [`stale`].
fn card(
    store: &GraphStore,
    session: &mut Session,
    root: &Path,
    file: &str,
    proposed: &str,
) -> Option<String> {
    let row = store.file_by_path(file).ok()??;
    let stored = store.symbols_in_file(row.id).ok()?;
    let fresh = pixel_graph::extract::extract_file(file, proposed.as_bytes())?.symbols;
    let mut out = String::new();
    for symbol in stored.iter().filter(|symbol| !symbol.sig.is_empty()) {
        let change = match fresh
            .iter()
            .find(|f| f.qualified == symbol.qualified && f.kind == symbol.kind)
        {
            Some(f) if shape(&f.sig) == shape(&symbol.sig) => continue,
            Some(f) => Change::Reshaped(shape(&f.sig)),
            None => Change::Removed,
        };
        let calls = callers(store, symbol, file).ok()?;
        if calls.is_empty() {
            continue;
        }
        let key = format!(
            "{}|{}",
            symbol.uid,
            match &change {
                Change::Reshaped(new) => new.as_str(),
                Change::Removed => "removed",
            }
        );
        if !session.carded.insert(key) {
            continue;
        }
        let (proved, possible): (Vec<&Caller>, Vec<&Caller>) =
            calls.iter().partition(|caller| caller.proved);
        push_bounded(
            &mut out,
            &format!(
                "[PIXEL:EDIT] `{}` ({}) in {file} {}; {} caller(s) outside it:",
                symbol.qualified,
                symbol.kind.as_str(),
                match &change {
                    Change::Reshaped(_) => "changes its signature",
                    Change::Removed => "is removed or renamed",
                },
                proved.len()
            ),
        );
        push_bounded(&mut out, &format!("  was: {}", shape(&symbol.sig)));
        if let Change::Reshaped(new) = &change {
            push_bounded(&mut out, &format!("  now: {new}"));
        }
        for caller in proved.iter().take(MAX_PROVED) {
            push_bounded(
                &mut out,
                &format!("  - {}:{} in {}", caller.file, caller.line, caller.holder),
            );
        }
        if !possible.is_empty() {
            push_bounded(&mut out, "  possible (name match only):");
            for caller in possible.iter().take(MAX_POSSIBLE) {
                push_bounded(
                    &mut out,
                    &format!("  - {}:{} in {}", caller.file, caller.line, caller.holder),
                );
            }
        }
        let sites: Vec<Site> = proved
            .iter()
            .filter(|caller| caller.file != file)
            .take(MAX_PROVED)
            .map(|caller| Site {
                file: caller.file.clone(),
                line: caller.line,
                holder: caller.holder.clone(),
                hash: hash_of(root, &caller.file),
            })
            .collect();
        session
            .pending
            .retain(|p| !(p.file == file && p.name == symbol.qualified));
        if !sites.is_empty() {
            session.pending.push(Pending {
                name: symbol.qualified.clone(),
                kind: symbol.kind.as_str().to_owned(),
                file: file.to_owned(),
                old: shape(&symbol.sig),
                announced: false,
                sites,
            });
        }
    }
    session.pending.truncate(MAX_PENDING);
    (!out.is_empty()).then(|| {
        push_bounded(
            &mut out,
            "Update these callers in the same change, or say why they are fine.",
        );
        out.trim_end().to_owned()
    })
}

/// The `[PIXEL:STALE-CALLERS]` note after `file` was written: the callers a
/// card named that are still unchanged. A caller file that was written since
/// leaves the set, and so does a declaration the write did not change after
/// all. Said once per declaration.
fn stale(session: &mut Session, root: &Path, file: &str) -> Option<String> {
    let now = hash_of(root, file);
    for pending in &mut session.pending {
        pending
            .sites
            .retain(|site| site.file != file || site.hash == now);
    }
    let source = read_source(root, file);
    let fresh = source.and_then(|text| pixel_graph::extract::extract_file(file, text.as_bytes()));
    for pending in session.pending.iter_mut().filter(|p| p.file == file) {
        let unchanged = fresh.as_ref().is_some_and(|extraction| {
            extraction
                .symbols
                .iter()
                .any(|s| s.qualified == pending.name && shape(&s.sig) == pending.old)
        });
        if unchanged {
            pending.sites.clear();
        }
    }
    session.pending.retain(|pending| !pending.sites.is_empty());
    let mut out = String::new();
    for pending in session
        .pending
        .iter_mut()
        .filter(|p| p.file == file && !p.announced)
    {
        pending.announced = true;
        push_bounded(
            &mut out,
            &format!(
                "[PIXEL:STALE-CALLERS] `{}` ({}) changed in {file}; these callers are unchanged:",
                pending.name, pending.kind
            ),
        );
        for site in pending.sites.iter().take(MAX_PROVED) {
            push_bounded(
                &mut out,
                &format!("  - {}:{} in {}", site.file, site.line, site.holder),
            );
        }
    }
    (!out.is_empty()).then(|| {
        push_bounded(&mut out, "Update them, or say why they are fine.");
        out.trim_end().to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shape_should_be_the_header_without_the_body_or_the_spacing() {
        for (sig, header) in [
            (
                "function alpha(x: number): number { return x + 1; }",
                "function alpha(x: number): number",
            ),
            (
                "function  alpha(x: number,\ty: number):   number {",
                "function alpha(x: number, y: number): number",
            ),
            (
                "const f = (x: { a: number }) => x.a;",
                "const f = (x: { a: number })",
            ),
            ("pub fn alpha(x: u8) -> u8 {", "pub fn alpha(x: u8) -> u8"),
            (
                "def alpha(x: int = {}) -> int: return x",
                "def alpha(x: int = {}) -> int",
            ),
            ("async def alpha(self):", "async def alpha(self)"),
            ("def alpha(x)", "def alpha(x)"),
            ("int alpha(int x);", "int alpha(int x)"),
            ("", ""),
        ] {
            assert_eq!(shape(sig), header, "{sig}");
        }
        assert_ne!(shape("fn a(x: u8) {"), shape("fn a(x: u8, y: u8) {"));
        assert_eq!(shape("fn a(x: u8) { 1 }"), shape("fn a(x: u8) { 2 }"));
        assert_ne!(
            shape("function a(): number { return 1 }"),
            shape("function a(): string { return 1 }")
        );
    }

    #[test]
    fn a_path_should_stay_inside_the_repository_and_off_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("src/a.rs"), root.join("src/link.rs")).unwrap();
        let rel = |path: &str| repo_relative(&root, &root.join("src"), path);
        assert_eq!(rel("a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(rel("../src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(
            rel(root.join("src/new.rs").to_str().unwrap()).as_deref(),
            Some("src/new.rs"),
            "a file the edit creates"
        );
        assert_eq!(rel("../../outside.rs"), None);
        assert_eq!(rel("../.git/config"), None);
        assert_eq!(rel("/etc/hosts"), None);
        assert_eq!(rel(""), None);
        #[cfg(unix)]
        assert_eq!(rel("link.rs"), None);
    }

    #[test]
    fn a_session_should_round_trip_and_ignore_what_it_cannot_read() {
        let temp = tempfile::tempdir().unwrap();
        let path = Session::path(temp.path(), "session-1");
        assert_eq!(Session::load(&path), Session::default());
        let mut session = Session::default();
        session.carded.insert("uid|change".to_owned());
        session.save(&path);
        assert_eq!(Session::load(&path), session);
        assert_ne!(path, Session::path(temp.path(), "session-2"));
        std::fs::write(&path, "{\"carded\": 3").unwrap();
        assert_eq!(Session::load(&path), Session::default());
        std::fs::write(&path, vec![b' '; SESSION_MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(Session::load(&path), Session::default());
    }

    #[test]
    fn a_text_should_stop_at_the_byte_bound() {
        let mut out = String::new();
        let line = "x".repeat(99);
        let mut pushed = 0;
        while push_bounded(&mut out, &line) {
            pushed += 1;
        }
        assert_eq!(pushed, TEXT_MAX_BYTES / 100);
        assert!(out.len() <= TEXT_MAX_BYTES);
    }
}
