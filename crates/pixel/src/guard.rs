//! `pixel hook guard` — mechanical enforcement of the sniper-targets
//! contract, ported from the original working `gitpixel-targets-guard`
//! Python hook (kept as `~/.claude/hooks/gitpixel-targets-guard.pixel-bak.*`
//! on this machine). The prior Rust implementation of this hook only
//! printed the manifest and never blocked anything — this replaces that
//! with real PreToolUse enforcement.
//!
//! Contract:
//! 1. SCOPING (ADVISORY) — while `<repo>/.pixel/targets.json` is active
//!    (younger than 24h), reads/greps/edits of repo files OUTSIDE the
//!    target list emit a NON-BLOCKING advisory note and proceed. The
//!    sniper-discovery benchmark (docs/bench/sniper-discovery.md) showed
//!    hard blocking collapses recall (0.60 → 0.19), so the fence advises
//!    instead of denying.
//! 2. MANDATE (ADVISORY) — in a pixel-indexed repo (a `.pixel` dir exists)
//!    with NO active manifest, edits to *existing* files get an advisory
//!    suggesting `pixel targets "<task>"` first; the edit proceeds. An
//!    EXPIRED manifest (>24h) gets an expiry advisory instead of a block.
//! 3. RESCUE (HARD BLOCK) — destructive git commands are denied with a
//!    pixel alternative: `git reset --hard/--keep`, raw historical file
//!    restores (`git checkout <ref> -- <path>`, `git restore --source`),
//!    `git clean -f*`, `git checkout -f/--force`, `git stash drop/clear`,
//!    `git branch -D`, `git push --force` (NOT `--force-with-lease`), and
//!    `git pull` (deny-with-suggestion: `pixel reconcile`, never executed
//!    on the agent's behalf). These denies run even when the command
//!    contains substitution/heredocs — a spurious deny costs a retry, a
//!    missed hard reset costs real work.
//! 4. GLOB — Glob tool calls are deliberately left un-denied: they only
//!    enumerate paths, and the Read/Edit of any result is itself guarded by
//!    the scoping rules above. Blocking enumeration would be pure noise.
//!
//! Rewrites NEVER change command semantics beyond read-only enrichment: a
//! rewrite must never add a write, push, or destructive step the original
//! command didn't have. `git pull` is therefore denied with a suggestion,
//! never rewritten into `pixel reconcile`.
//!
//! Blocks by exiting 2 with a corrective message on stderr (the exit code
//! Claude Code's hook protocol treats as "deny, feed stderr to the model").
//! Advisories exit 0 with a JSON note (systemMessage + additionalContext),
//! no permissionDecision — the normal permission flow is untouched.
//! Fails open (exit 0) on any parse error or unexpected shape — a guard
//! that crashes or wedges the session is worse than a guard that misses a
//! case.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const MANIFEST_MAX_AGE_SECS: u64 = 24 * 3600;
/// Wall-clock budget for the in-hook `pixel search` child that powers
/// deny-with-answer. Past this, fall back to the suggestion-only message.
const SEARCH_ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// Caps applied to inline search results embedded in a deny message.
const SEARCH_ANSWER_MAX_LINES: usize = 80;
const SEARCH_ANSWER_MAX_BYTES: usize = 8 * 1024;
const ORIENTATION_ANY: &[&str] = &["CLAUDE.md", "AGENTS.md", "README.md"];
const ORIENTATION_ROOT: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "tsconfig.json",
    ".gitignore",
];
const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "nl", "sed", "awk", "rg", "grep", "egrep",
    "fgrep", "find", "strings", "wc",
];

/// One scoped task inside the manifest. v2 manifests carry several of
/// these (concurrent agents each scope their own task); the legacy v1
/// shape maps to exactly one.
struct TaskEntry {
    task: String,
    files: Vec<(String, String)>, // (path, tier)
}

struct Manifest {
    root: PathBuf,
    tasks: Vec<TaskEntry>,
}

/// Entry point for `pixel hook guard`. Reads the PreToolUse hook payload
/// from stdin. Never returns an `Err` that would surface as exit 1 — every
/// failure path is a deliberate exit 0 (allow) or exit 2 (block).
pub fn run() -> ! {
    if let Ok(kill) = std::env::var("PIXEL_TARGETS_GUARD") {
        if matches!(kill.as_str(), "0" | "false" | "off") {
            std::process::exit(0);
        }
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        std::process::exit(0);
    }
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        std::process::exit(0);
    };
    if !payload.is_object() {
        std::process::exit(0);
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_guard_event(event) {
        std::process::exit(0);
    }

    let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(tool_input) = tool_input.as_object() else {
        std::process::exit(0);
    };

    let raw_path = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let anchor = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));

    let idx_root = find_up(&anchor, ".pixel");
    let manifest_root = find_up(&anchor, &Path::new(".pixel").join("targets.json"));
    let (manifest, manifest_expired) = match manifest_root.as_deref().map(load_manifest_state) {
        Some(ManifestState::Active(m)) => (Some(m), false),
        Some(ManifestState::Expired) => (None, true),
        _ => (None, false),
    };

    if tool == "Bash" || tool == "exec" || tool == "bash" || tool == "run_shell_command" || tool == "execute" {
        let cmd = tool_input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Policy first, rewrite second: only commands that already passed
        // check_bash may be transparently rewritten to a pixel equivalent.
        // If a rewrite applies, emit the updatedInput JSON and exit 0 — the
        // agent receives pixel's enriched output without knowing the command
        // was rewritten.
        check_bash(cmd, &cwd, idx_root.as_deref(), manifest.as_ref());
        if idx_root.is_some() {
            if let Some(rewritten) = try_rewrite_bash(cmd, &cwd) {
                allow_rewrite(&rewritten);
            }
        }
        std::process::exit(0);
    }

    match tool {
        "Read" | "Grep" | "Glob"
        | "read" | "grep" | "find_file_by_name" | "glob" | "notebook_read"
        | "read_file" | "search" => {
            // In indexed repos, redirect Grep tool calls to pixel search.
            // Can't rewrite the tool type (Grep→Bash), so deny with a message
            // that tells the agent exactly what to run instead.
            if idx_root.is_some() && is_grep_tool(tool, &tool_input) {
                let pattern = tool_input
                    .get("pattern")
                    .and_then(Value::as_str)
                    .or_else(|| tool_input.get("query").and_then(Value::as_str))
                    .unwrap_or("");
                if !pattern.is_empty() {
                    grep_redirect(&pattern, &cwd, &tool_input);
                }
            }
            if let Some(m) = &manifest {
                let p = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));
                if !allowed(&p, m) {
                    scoping_advisory(&p, m);
                }
            }
        }
        "Edit" | "MultiEdit" | "NotebookEdit" | "Write"
        | "edit" | "write" | "notebook_edit"
        | "apply_patch" | "write_file" => {
            let Some(p) = resolve(raw_path, &cwd) else {
                std::process::exit(0);
            };
            let exists = p.is_file();
            if (tool == "Write" || tool == "write" || tool == "write_file") && !exists {
                std::process::exit(0); // creating a new file is always allowed
            }
            if let Some(m) = &manifest {
                if exists && !allowed(&p, m) {
                    scoping_advisory(&p, m);
                }
                std::process::exit(0);
            }
            // MANDATE (advisory) — indexed repo, no active manifest: note
            // that the edit is unscoped, but let it proceed.
            if let Some(root) = &idx_root {
                if exists && !is_exempt(&p, root) {
                    if manifest_expired {
                        expired_manifest_advisory(root);
                    }
                    mandate_advisory(&p, root);
                }
            } else if exists {
                // Unindexed git repo: suggest indexing so pixel's scoped
                // retrieval works. Advisory only — the edit proceeds.
                if let Some(git_root) = find_up(&anchor, ".git") {
                    suggest_index_advisory(&git_root);
                }
            }
        }
        _ => {}
    }
    std::process::exit(0);
}

/// Accept both Claude Code's PreToolUse and Gemini's BeforeTool hook events.
fn is_guard_event(event: &str) -> bool {
    event == "PreToolUse" || event == "BeforeTool"
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn resolve(raw: &str, base: &Path) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    Some(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// Walk upward from `start` (or its parent, if `start` is a file) looking
/// for `rel` (a file or directory). Returns the directory containing it.
fn find_up(start: &Path, rel: impl AsRef<Path>) -> Option<PathBuf> {
    let rel = rel.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join(rel).exists() {
            return Some(dir);
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Outcome of reading `.pixel/targets.json`: distinguishes "no usable
/// manifest because everything hit the 24h TTL" (worth an advisory note)
/// from "no manifest at all / unreadable" (silent).
enum ManifestState {
    Absent,
    Expired,
    Active(Manifest),
}

/// Read the enforcement manifest, accepting BOTH shapes:
/// - v2 (multi-task): `{version: 2, tasks: [{id, task, created_unix, targets: [...]}]}`
/// - legacy (v1/singleton): `{task, created_unix, files: [...]}`
/// Expired tasks (older than the 24h TTL) are dropped individually; a
/// manifest whose tasks have all expired reports `Expired`.
fn load_manifest_state(root: &Path) -> ManifestState {
    let Ok(text) = std::fs::read_to_string(root.join(".pixel").join("targets.json")) else {
        return ManifestState::Absent;
    };
    let Ok(m) = serde_json::from_str::<Value>(&text) else {
        return ManifestState::Absent;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return ManifestState::Absent;
    };
    let now = now.as_secs();
    let mut saw_expired = false;
    let tasks: Vec<TaskEntry> = if m.get("version").and_then(Value::as_u64) == Some(2) {
        let Some(raw_tasks) = m.get("tasks").and_then(Value::as_array) else {
            return ManifestState::Absent;
        };
        raw_tasks
            .iter()
            .filter(|t| {
                let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
                let fresh = now.saturating_sub(created) <= MANIFEST_MAX_AGE_SECS;
                if !fresh {
                    saw_expired = true;
                }
                fresh
            })
            .filter_map(|t| {
                Some(TaskEntry {
                    task: t.get("task").and_then(Value::as_str).unwrap_or("?").to_string(),
                    files: parse_manifest_files(t.get("targets")?.as_array()?),
                })
            })
            .collect()
    } else {
        let created_unix = m.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        if now.saturating_sub(created_unix) > MANIFEST_MAX_AGE_SECS {
            return ManifestState::Expired;
        }
        let Some(files) = m.get("files").and_then(Value::as_array) else {
            return ManifestState::Absent;
        };
        vec![TaskEntry {
            task: m.get("task").and_then(Value::as_str).unwrap_or("?").to_string(),
            files: parse_manifest_files(files),
        }]
    };
    if tasks.is_empty() {
        return if saw_expired {
            ManifestState::Expired
        } else {
            ManifestState::Absent
        };
    }
    ManifestState::Active(Manifest { root: root.to_path_buf(), tasks })
}

/// Compatibility shim over `load_manifest_state` for tests that only care
/// about an active manifest.
#[cfg(test)]
fn load_manifest(root: &Path) -> Option<Manifest> {
    match load_manifest_state(root) {
        ManifestState::Active(m) => Some(m),
        _ => None,
    }
}

fn parse_manifest_files(raw: &[Value]) -> Vec<(String, String)> {
    raw.iter()
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?.to_string();
            let tier = f.get("tier").and_then(Value::as_str).unwrap_or("").to_string();
            Some((path, tier))
        })
        .collect()
}

/// Iterator over every (path, tier) across all active tasks.
fn all_files(m: &Manifest) -> impl Iterator<Item = &(String, String)> {
    m.tasks.iter().flat_map(|t| t.files.iter())
}

fn rel_of<'a>(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

/// Scoping verdict for one absolute path while a manifest is active.
fn allowed(abs: &Path, m: &Manifest) -> bool {
    if abs != m.root && !abs.starts_with(&m.root) {
        return true; // outside the scoped repo entirely
    }
    let rel = rel_of(abs, &m.root);
    if rel == ".pixel" || rel.starts_with(".pixel/") {
        return true;
    }
    let target_paths: HashSet<&str> = all_files(m).map(|(p, _)| p.as_str()).collect();
    if target_paths.contains(rel.as_str()) {
        return true;
    }
    if abs.is_dir() {
        if rel.is_empty() || rel == "." {
            return true;
        }
        let prefix = format!("{rel}/");
        return target_paths.iter().any(|t| t.starts_with(&prefix));
    }
    let basename = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if ORIENTATION_ANY.contains(&basename) {
        return true;
    }
    if ORIENTATION_ROOT.contains(&rel.as_str()) {
        return true;
    }
    false
}

fn is_exempt(abs: &Path, idx_root: &Path) -> bool {
    if abs != idx_root && !abs.starts_with(idx_root) {
        return true; // outside the indexed repo
    }
    let rel = rel_of(abs, idx_root);
    if rel.starts_with(".pixel/") {
        return true;
    }
    let basename = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
    ORIENTATION_ANY.contains(&basename) || ORIENTATION_ROOT.contains(&rel.as_str())
}

fn block(lines: &[String]) -> ! {
    eprintln!("{}", lines.join("\n"));
    std::process::exit(2);
}

/// Build the NON-BLOCKING advisory response JSON. Deliberately carries NO
/// `permissionDecision`: the tool call proceeds through the normal
/// permission flow; the note is surfaced to the user (`systemMessage`) and
/// offered to the model (`additionalContext`).
fn advisory_json(note: &str) -> Value {
    serde_json::json!({
        "systemMessage": note,
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": note
        }
    })
}

/// Emit a non-blocking advisory and allow the tool call (exit 0).
fn advise(lines: &[String]) -> ! {
    print!("{}", advisory_json(&lines.join("\n")));
    std::process::exit(0);
}

/// Truncate a task string for display (char-safe, appends an ellipsis).
fn short_task(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Advisory note for a read/edit outside the active targets manifest.
/// Non-blocking by design: the sniper-discovery benchmark showed hard
/// scoping denies collapse task recall, so the fence informs instead.
fn scoping_advisory_lines(abs: &Path, m: &Manifest) -> Vec<String> {
    let rel = rel_of(abs, &m.root);
    let total: usize = m.tasks.iter().map(|t| t.files.len()).sum();
    let mut lines = vec![format!(
        "pixel-targets-guard advisory: '{rel}' is outside the active targets manifest ({} task(s), {total} file(s)):",
        m.tasks.len()
    )];
    lines.extend(
        m.tasks
            .iter()
            .map(|t| format!("  - '{}'", short_task(&t.task, 70))),
    );
    lines.push(
        "Proceeding. If scope has drifted, re-run `pixel targets \"<refined task>\"`".into(),
    );
    lines.push(
        "to refresh your task's list, or `pixel targets --clear` to end scoping.".into(),
    );
    lines
}

fn scoping_advisory(abs: &Path, m: &Manifest) -> ! {
    advise(&scoping_advisory_lines(abs, m));
}

/// Advisory note for an edit in an indexed repo with no active manifest.
fn mandate_advisory_lines(abs: &Path, idx_root: &Path) -> Vec<String> {
    let rel = rel_of(abs, idx_root);
    vec![
        "pixel-targets-guard advisory: no sniper target list is active for this repo.".into(),
        format!("Proceeding with this edit ({rel}), but scoping first is recommended:"),
        "  pixel targets \"<one-line task description>\" .".into(),
        "That returns the closed P0/P1/P2 file list and activates .pixel/targets.json.".into(),
        "Ending a task: pixel targets --clear".into(),
    ]
}

fn mandate_advisory(abs: &Path, idx_root: &Path) -> ! {
    advise(&mandate_advisory_lines(abs, idx_root));
}

/// Advisory note when the targets manifest exists but every task in it has
/// exceeded the 24h TTL.
fn expired_manifest_advisory_lines(idx_root: &Path) -> Vec<String> {
    vec![
        format!(
            "pixel-targets-guard advisory: the targets manifest in {} has expired (24h TTL).",
            idx_root.join(".pixel").join("targets.json").display()
        ),
        "Proceeding unscoped. If you are still working a scoped task, re-run".into(),
        "  pixel targets \"<one-line task description>\" .".into(),
    ]
}

fn expired_manifest_advisory(idx_root: &Path) -> ! {
    advise(&expired_manifest_advisory_lines(idx_root));
}

/// Advisory for edits in a git repo that pixel hasn't indexed yet: suggest
/// indexing so scoped retrieval works, then proceed.
fn suggest_index_advisory(git_root: &Path) -> ! {
    advise(&[
        "pixel-targets-guard advisory: this is a git repo but pixel has not indexed it.".into(),
        "Proceeding. To enable pixel's scoped retrieval (one-time, takes seconds):".into(),
        format!("  pixel index {}", git_root.display()),
        "Then scope tasks with: pixel targets \"<one-line task description>\" .".into(),
    ]);
}

/// Bash-command checks. Destructive-git DENIES run first and are NOT
/// skipped for commands containing substitution/heredocs — `git reset
/// --hard $(git rev-parse HEAD~1)` is exactly as destructive as the
/// literal form. A heredoc body that merely *mentions* a destructive
/// command can false-positive here; that is accepted for this class,
/// because a spurious deny costs one retry while a missed hard reset
/// costs real work. Everything below the deny tier (scoping advisories,
/// reader-target detection) stays conservative and skips substituted
/// commands.
fn check_bash(cmd: &str, cwd: &Path, idx_root: Option<&Path>, manifest: Option<&Manifest>) {
    if let Some(lines) = bash_deny_lines(cmd, idx_root) {
        block(&lines);
    }
    if cmd.contains("<<") || cmd.contains("$(") || cmd.contains('`') {
        return;
    }
    if let Some(m) = manifest {
        if let Some(first_file) = single_reader_target(cmd, cwd) {
            if !allowed(&first_file, m) {
                scoping_advisory(&first_file, m);
            }
        }
    }
}

/// Hard-deny tier for Bash commands: destructive git operations. Returns
/// the deny message lines, or None to allow. Deliberately has NO
/// substitution/heredoc bail — see `check_bash`.
fn bash_deny_lines(cmd: &str, idx_root: Option<&Path>) -> Option<Vec<String>> {
    let root = idx_root?;
    if !cmd.contains("git") {
        return None;
    }
    for (sub, args) in git_invocations(cmd) {
        if let Some(lines) = destructive_git_deny(&sub, &args, root) {
            return Some(lines);
        }
    }
    None
}

/// Split a shell command into pipeline/sequence segments and extract every
/// `git <subcommand> <args…>` invocation as owned tokens. Uses the guard's
/// simple quote-aware tokenizer — not a full shell parser, but robust to
/// flag ordering and to substitution-wrapped arguments (a `$(…)` chunk
/// becomes ordinary tokens that simply never match a destructive flag).
fn git_invocations(cmd: &str) -> Vec<(String, Vec<String>)> {
    let normalized = cmd.replace("&&", ";").replace("||", ";");
    let mut out = Vec::new();
    for segment in normalized.split([';', '|', '\n']) {
        let tokens = simple_tokenize(segment.trim());
        let Some(git_pos) = tokens.iter().position(|t| t == "git") else {
            continue;
        };
        let mut rest = tokens[git_pos + 1..].iter();
        let mut sub = None;
        while let Some(t) = rest.next() {
            if t == "-C" || t == "-c" {
                let _ = rest.next(); // skip the global flag's value
                continue;
            }
            if t.starts_with('-') {
                continue; // other global flags (--no-pager, --git-dir=…)
            }
            sub = Some(t.clone());
            break;
        }
        if let Some(sub) = sub {
            out.push((sub, rest.cloned().collect()));
        }
    }
    out
}

/// True for a combined short-flag cluster containing `c` (e.g. `-fd`
/// contains 'f', `-Df` contains 'D'). Long flags (`--force`) don't match.
fn short_cluster_has(token: &str, c: char) -> bool {
    token.len() >= 2
        && token.starts_with('-')
        && !token.starts_with("--")
        && token[1..].chars().all(|ch| ch.is_ascii_alphanumeric())
        && token[1..].contains(c)
}

/// Deny verdict for one parsed `git <sub> <args>` invocation. Flag-order
/// robust: matching is on tokens, not raw substrings.
fn destructive_git_deny(sub: &str, args: &[String], root: &Path) -> Option<Vec<String>> {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let cluster = |c: char| args.iter().any(|a| short_cluster_has(a, c));
    match sub {
        "reset" if has("--hard") || has("--keep") => Some(vec![
            "BLOCKED by pixel-targets-guard: `git reset --hard/--keep` destroys in-progress work.".into(),
            "\"It was working before\" is a rescue problem — use the surgical planner:".into(),
            "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
            "  pixel rescue --apply <oid> --file <path>  # gated restore (working tree only)".into(),
            "Dirty files: add --merge (3-way, keeps your edits) or --stash-first.".into(),
        ]),
        "checkout" if has("--") => Some(raw_restore_deny()),
        "checkout" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git checkout -f/--force` discards in-progress work.".into(),
            "Use the surgical planner instead:".into(),
            "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
            "  pixel rescue --apply <oid> --file <path> [--merge|--stash-first]".into(),
        ]),
        "restore" if args.iter().any(|a| a == "--source" || a.starts_with("--source=")) => {
            Some(raw_restore_deny())
        }
        "clean" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git clean -f` permanently deletes untracked files.".into(),
            "If something went missing, recover it instead of deleting more:".into(),
            "  pixel excavate --phrase \"<what you're looking for>\"  # history/stash/reflog search".into(),
            "  pixel rescue \"<what broke>\" .".into(),
        ]),
        "stash" if args.first().is_some_and(|a| a == "drop" || a == "clear") => Some(vec![
            "BLOCKED by pixel-targets-guard: `git stash drop/clear` permanently discards stashed work.".into(),
            "Stashed code is recoverable history — use:".into(),
            "  pixel excavate --phrase \"<what you're looking for>\"  # searches stash + reflog too".into(),
        ]),
        "branch" if has("-D") || cluster('D') || (has("--delete") && (has("--force") || cluster('f'))) => {
            Some(vec![
                "BLOCKED by pixel-targets-guard: `git branch -D` force-deletes unmerged work.".into(),
                "If the branch's code matters, recover it deliberately:".into(),
                "  pixel excavate --phrase \"<what you're looking for>\"".into(),
                "  pixel rescue \"<what broke>\" .".into(),
            ])
        }
        // `--force-with-lease` (and `--force-if-includes`) are the safe
        // forms pixel's own ops use — only bare `--force`/`-f` is denied.
        "push" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git push --force` can destroy remote history.".into(),
            "Use pixel's gated mutation ops instead:".into(),
            format!("  pixel push --request-id <id> {}", shell_quote(&root.display().to_string())),
            format!("  pixel ship --files <f>... --message \"<msg>\" --request-id <id> {}", shell_quote(&root.display().to_string())),
            "(pixel push uses --force-with-lease semantics only where safe.)".into(),
        ]),
        // `git pull` is denied with a suggestion, NEVER rewritten: a
        // transparent substitute would discard remote/branch args and a
        // `--push` default would add a write the original didn't have.
        "pull" => Some(vec![
            "BLOCKED by pixel-targets-guard: raw `git pull` (fetch + merge) is replaced by deterministic reconciliation.".into(),
            "Run instead:".into(),
            format!(
                "  pixel reconcile {} --strategy rebase-if-clean",
                shell_quote(&root.display().to_string())
            ),
            "It fetches, proves a clean rebase via merge-tree before touching the worktree,".into(),
            "and reports structured conflicts when they exist. It does not push.".into(),
        ]),
        _ => None,
    }
}

fn raw_restore_deny() -> Vec<String> {
    vec![
        "BLOCKED by pixel-targets-guard: raw historical file restore can clobber in-progress work.".into(),
        "Use the surgical planner instead:".into(),
        "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
        "  pixel rescue --apply <oid> --file <path> [--merge|--stash-first]".into(),
    ]
}

/// If `cmd`'s first pipeline segment is a known reader command with exactly
/// one existing-file argument, resolve and return it. Bails (returns
/// `None`) on anything containing command substitution, backticks,
/// heredocs, or loop keywords — those are too complex to reason about
/// conservatively, so they're simply not checked (fail open).
fn single_reader_target(cmd: &str, cwd: &Path) -> Option<PathBuf> {
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains("<<") {
        return None;
    }
    if ["xargs", "for ", "while "].iter().any(|kw| cmd.contains(kw)) {
        return None;
    }
    let first_segment = cmd
        .split([';', '|'])
        .next()?
        .split("&&")
        .next()?
        .trim();
    let tokens = simple_tokenize(first_segment);
    let (tokens, eff_cwd) = if tokens.first().map(String::as_str) == Some("cd") {
        let rest_after_cd = cmd.splitn(2, "&&").nth(1)?.trim();
        let new_cwd = resolve(tokens.get(1)?, cwd)?;
        let rest_tokens = simple_tokenize(rest_after_cd.split([';', '|']).next()?.trim());
        (rest_tokens, new_cwd)
    } else {
        (tokens, cwd.to_path_buf())
    };
    let cmd_name = tokens.first()?.as_str();
    if !READERS.contains(&cmd_name) {
        return None;
    }
    let mut args: Vec<&str> = tokens[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if matches!(cmd_name, "sed" | "awk") && !args.is_empty() {
        args.remove(0); // the sed/awk program itself, not a file
    }
    let files: Vec<PathBuf> = args
        .iter()
        .filter_map(|a| resolve(a, &eff_cwd))
        .filter(|p| p.is_file())
        .collect();
    if files.len() == 1 {
        Some(files.into_iter().next().unwrap())
    } else {
        None
    }
}

/// Minimal whitespace tokenizer honoring single/double quotes. Not a full
/// shell parser — sufficient for the conservative reader-file detection
/// above, matching the original hook's own scope.
fn simple_tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Command rewriting — transparent upgrade of grep/rg/git to pixel equivalents.
// Modeled on RTK's rewrite approach: the hook returns updatedInput JSON and
// the agent receives pixel's enriched output without knowing the command was
// rewritten. Only fires in indexed repos (.pixel/ exists).
// ---------------------------------------------------------------------------

/// Emit a PreToolUse "allow" response with a rewritten Bash command. The
/// agent receives pixel's output instead of the original tool's output.
fn allow_rewrite(new_command: &str) -> ! {
    // Deliberately NO permissionDecision:"allow": the rewritten command must
    // still go through normal permission evaluation, so the agent sees and
    // approves the pixel command it is about to run.
    let resp = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": {
                "command": new_command
            }
        }
    });
    print!("{}", resp);
    std::process::exit(0);
}

/// Check if a tool call is a Grep-style search (has a pattern/query field).
fn is_grep_tool(tool: &str, input: &serde_json::Map<String, Value>) -> bool {
    // Claude Code's Grep tool has "pattern"; Devin's grep has "pattern";
    // some agents use "query". Read/Glob don't have pattern fields.
    if tool != "Grep" && tool != "grep" && tool != "search" {
        return false;
    }
    input.get("pattern").is_some() || input.get("query").is_some()
}

/// Deny a Grep tool call with a message redirecting to `pixel search` —
/// but only when the search is actually equivalent. If the Grep tool
/// carries fields pixel search can't express (glob/type/output_mode), we
/// ALLOW THROUGH: a deny with a non-equivalent suggestion is worse than no
/// guard. Returns true if it blocked (never returns), false to allow.
fn grep_redirect(
    pattern: &str,
    cwd: &Path,
    input: &serde_json::Map<String, Value>,
) -> bool {
    // Context flags are expressible; glob/type/output_mode are not.
    let mut flags = Vec::new();
    for f in ["-A", "-B", "-C"] {
        if input.contains_key(f) {
            flags.push(f.to_string());
        }
    }
    if input.contains_key("glob") || input.contains_key("type") || input.contains_key("output_mode") {
        return false;
    }
    let root = find_up(cwd, ".pixel")
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    let Some(cmd) = search_can_replace(pattern, &flags, &root) else {
        return false;
    };
    // Deny-with-answer: run the equivalent pixel search HERE and embed the
    // results in the deny message, so the agent doesn't burn a full LLM
    // round-trip re-issuing the search itself. On any child failure or
    // timeout, fall back to the suggestion-only message.
    let results = run_pixel_search(pattern, &root, SEARCH_ANSWER_TIMEOUT);
    block(&grep_deny_lines(pattern, &cmd, results.as_deref()));
}

/// Build the deny message for a Grep redirect. With `results`, the answer
/// is inlined; without, the message only suggests the pixel command.
fn grep_deny_lines(pattern: &str, cmd: &str, results: Option<&str>) -> Vec<String> {
    match results {
        Some(out) => vec![
            format!(
                "BLOCKED Grep — here are the pixel search results for '{pattern}' instead:"
            ),
            truncate_results(out),
            format!("(Use these results; for a different query run: {cmd})"),
        ],
        None => vec![
            "BLOCKED by pixel-guard: use pixel search instead of Grep in indexed repos."
                .into(),
            format!("Run this via Bash: {cmd}"),
            "pixel search returns the match + surrounding code (no follow-up Read needed)."
                .into(),
        ],
    }
}

/// Cap inline search results to `SEARCH_ANSWER_MAX_LINES` lines and
/// `SEARCH_ANSWER_MAX_BYTES` bytes (whichever bites first), noting the cut.
fn truncate_results(out: &str) -> String {
    let mut kept = String::new();
    let mut truncated = false;
    for (i, line) in out.lines().enumerate() {
        if i >= SEARCH_ANSWER_MAX_LINES
            || kept.len() + line.len() + 1 > SEARCH_ANSWER_MAX_BYTES
        {
            truncated = true;
            break;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    if truncated {
        kept.push_str("\n  … (results truncated — run the pixel search yourself for the rest)");
    }
    kept
}

/// Run `pixel search '<pattern>' <root> --context 5` as a child of this
/// very binary and capture stdout. Returns None on spawn failure, non-zero
/// exit, empty output, or timeout — the caller then falls back to the
/// suggestion-only deny. Never panics, never hangs past `timeout`.
fn run_pixel_search(pattern: &str, root: &str, timeout: Duration) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    run_search_child(&exe, pattern, root, timeout)
}

/// Testable core of `run_pixel_search`: exec `exe` with search args. The
/// child's stdout is drained on a dedicated thread so a chatty child can
/// never deadlock the pipe while we poll for exit/timeout.
fn run_search_child(
    exe: &Path,
    pattern: &str,
    root: &str,
    timeout: Duration,
) -> Option<String> {
    let mut child = std::process::Command::new(exe)
        .args(["search", pattern, root, "--context", "5"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = rx.recv_timeout(Duration::from_millis(500)).ok()?;
                let _ = reader.join();
                if status.success() && !out.trim().is_empty() {
                    return Some(out);
                }
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Try to rewrite a Bash command to a pixel equivalent. Returns the new
/// command string if a rewrite applies, or None to let the original pass.
fn try_rewrite_bash(cmd: &str, cwd: &Path) -> Option<String> {
    let trimmed = cmd.trim();

    // Skip complex commands — heredocs, command substitution are left alone.
    if trimmed.contains("<<")
        || trimmed.contains("$(")
        || trimmed.contains('`')
    {
        return None;
    }

    // Strip a leading `cd <dir> &&` prefix — agents commonly generate
    // `cd /path && grep ...`. The cd changes the cwd for the grep, so we
    // resolve the new cwd and pass it to the grep rewriter. The rest of
    // the command (after &&) is what we actually rewrite.
    let (effective_cwd, body) = strip_cd_prefix(trimmed, cwd);

    let root_dir = find_up(&effective_cwd, ".pixel").unwrap_or_else(|| effective_cwd.clone());
    let root = root_dir.display().to_string();

    // After stripping cd, check for remaining control operators (&, ;, >, <)
    // that we can't handle. Pipes (|) are handled below.
    if has_unquoted_control(body) {
        return None;
    }

    // --- rg / grep → pixel search ---
    // Handle pipelines: if the command is `grep ... | grep -v ... | sort`,
    // try to rewrite the FIRST segment (before the first `|`). If the first
    // segment is a grep/rg that can be replaced by `pixel search`, rewrite
    // just that segment and keep the rest of the pipeline intact. This is
    // the common pattern agents generate: `grep -rln "pattern" ... | grep -v
    // node_modules | sort | wc -l`.
    if let Some(pipe_idx) = first_unquoted_pipe(body) {
        let first_segment = body[..pipe_idx].trim();
        let rest = &body[pipe_idx + 1..];
        if let Some(rewritten) = try_rewrite_grep(first_segment, &effective_cwd, &root_dir) {
            // Re-attach the cd prefix if we stripped one, so the rewritten
            // command still runs in the right directory for the pipeline
            // filters that follow.
            if body.len() != trimmed.len() {
                let cd_prefix = &trimmed[..trimmed.len() - body.len()];
                return Some(format!("{cd_prefix}{rewritten} |{rest}"));
            }
            return Some(format!("{rewritten} |{rest}"));
        }
        // First segment isn't a grep — don't touch the pipeline.
        return None;
    }

    if let Some(rewritten) = try_rewrite_grep(body, &effective_cwd, &root_dir) {
        if body.len() != trimmed.len() {
            let cd_prefix = &trimmed[..trimmed.len() - body.len()];
            return Some(format!("{cd_prefix}{rewritten}"));
        }
        return Some(rewritten);
    }

    // --- git log with search intent → pixel excavate ---
    if let Some(rewritten) = try_rewrite_git_archaeology(body, &root) {
        if body.len() != trimmed.len() {
            let cd_prefix = &trimmed[..trimmed.len() - body.len()];
            return Some(format!("{cd_prefix}{rewritten}"));
        }
        return Some(rewritten);
    }

    None
}

/// Strip a leading `cd <dir> && ` prefix from a command, returning the
/// effective cwd (original cwd + cd target) and the remaining body. If
/// there's no cd prefix, returns (original_cwd, original_cmd).
fn strip_cd_prefix<'a>(cmd: &'a str, cwd: &Path) -> (PathBuf, &'a str) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("cd ") {
        return (cwd.to_path_buf(), cmd);
    }
    // Find the first unquoted `&&` after the cd.
    let rest_after_cd = &trimmed[3..];
    let amp_idx = match find_unquoted_double_amp(rest_after_cd) {
        Some(i) => i,
        None => return (cwd.to_path_buf(), cmd),
    };
    let dir_str = rest_after_cd[..amp_idx].trim();
    // Strip quotes from the directory.
    let dir_str = dir_str
        .trim_matches(|c| c == '\'' || c == '"')
        .trim();
    let new_cwd = if dir_str.starts_with('/') {
        PathBuf::from(dir_str)
    } else {
        cwd.join(dir_str)
    };
    let body = rest_after_cd[amp_idx + 2..].trim_start();
    // Return the body with a reference into the original string.
    // Find where body starts in the original cmd.
    let body_offset = cmd.len() - body.len();
    let body_ref = &cmd[body_offset..];
    (new_cwd, body_ref)
}

/// Find the byte index of the first unquoted `&&` in the string.
fn find_unquoted_double_amp(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' && chars[i + 1] == '&' => {
                return Some(s.char_indices().nth(i).map(|(idx, _)| idx).unwrap_or(0));
            }
            None => {}
        }
        i += 1;
    }
    None
}

/// Flags that consume a following value (or an attached `=value`), so they
/// must be skipped when locating the search pattern.
const VALUE_FLAGS: &[&str] = &[
    "-A", "-B", "-C", "-m", "-g", "-t", "-f", "--include", "--exclude",
    "--glob", "--type", "-d", "--max-depth",
];

/// Value-consuming flags that also change the match count in ways `pixel
/// search` can't reproduce. Their presence makes a rewrite non-equivalent,
/// so the command falls through to the original. File-filter flags
/// (`--include`/`--exclude`/`--glob`/`--type`) are NOT here — we drop them
/// and search a superset (see `search_can_replace`).
const SCOPE_FLAGS: &[&str] = &[
    "-m",
];

/// Check for unquoted control operators EXCEPT pipe (`|`) and redirects
/// (`>`, `<`). Pipes are handled separately by [`first_unquoted_pipe`].
/// Redirects (`2>/dev/null`, `> out.txt`) are common in grep commands and
/// don't change the command structure — the guard can safely rewrite the
/// grep part and leave the redirect in place. `&&` is handled by
/// [`strip_cd_prefix`] which strips a leading `cd X &&` before this check.
fn has_unquoted_control(cmd: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut prev_amp = false;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' => {
                // Single `&` (background) is control; `&&` is handled by
                // strip_cd_prefix for the leading cd case. A `&&` in the
                // middle of the body (after cd strip) IS control.
                if prev_amp {
                    return true; // `&&` in the body
                }
                prev_amp = true;
                continue;
            }
            None if c == ';' || c == '\n' => return true,
            None => {}
        }
        prev_amp = false;
    }
    false
}

/// Find the byte index of the first unquoted pipe (`|`) in the command, or
/// None if there are no unquoted pipes. Used to split a pipeline into
/// segments so the first grep/rg segment can be rewritten to `pixel search`
/// while keeping the rest of the pipe intact.
fn first_unquoted_pipe(cmd: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_was_pipe = false;
    for (i, c) in cmd.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '|' => {
                // Skip `||` (logical OR) — only split on a single `|` pipe.
                if prev_was_pipe {
                    prev_was_pipe = false;
                    continue;
                }
                // Look ahead: is the next char also `|`? Then it's `||`.
                if cmd[i + 1..].starts_with('|') {
                    prev_was_pipe = true;
                    continue;
                }
                return Some(i);
            }
            None => {}
        }
        prev_was_pipe = false;
    }
    None
}

/// Single-quote `s` for shell interpolation, leaving it bare when it is
/// already shell-safe (so common roots like `/repo` stay readable).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '/' | '.' | '_' | '-' | ':' | '=' | '+' | '@' | '~')
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse a grep/rg command into (pattern, path-scope args, unsupported
/// flags). Returns None if the command isn't a grep-style search.
fn parse_grep(cmd: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return None;
    }
    let bin = tokens[0].as_str();
    if !matches!(bin, "rg" | "grep" | "egrep" | "fgrep") {
        return None;
    }
    let unsupported_flags = [
        "-l", "--files-with-matches", "-c", "--count", "-v", "--invert",
        "-o", "--only-matching",
    ];
    let mut unsupported: Vec<String> = tokens[1..]
        .iter()
        .filter(|t| unsupported_flags.contains(&t.as_str()))
        .cloned()
        .collect();
    // Locate the pattern, skipping value-consuming flags and their values.
    let mut i = 1;
    let mut pattern: Option<String> = None;
    let mut pattern_idx = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "-e" {
            pattern = tokens.get(i + 1).cloned();
            pattern_idx = i + 1;
            break;
        }
        if let Some(p) = t.strip_prefix("--regexp=") {
            pattern = Some(p.to_string());
            pattern_idx = i;
            break;
        }
        if t.starts_with('-') {
            if t.starts_with("--") && t.contains('=') {
                let base = t.split('=').next().unwrap_or(t);
                if SCOPE_FLAGS.contains(&base) {
                    unsupported.push(base.to_string());
                }
                i += 1; // self-contained --flag=value
                continue;
            }
            if VALUE_FLAGS.contains(&t.as_str()) {
                if SCOPE_FLAGS.contains(&t.as_str()) {
                    unsupported.push(t.clone());
                }
                i += 2; // flag + its value
                continue;
            }
            if t.len() > 2 && !t.starts_with("--") {
                let flag = &t[..2];
                if VALUE_FLAGS.contains(&flag) {
                    if SCOPE_FLAGS.contains(&flag) {
                        unsupported.push(flag.to_string());
                    }
                    i += 1; // short flag with attached value, e.g. -A5
                    continue;
                }
            }
            i += 1;
            continue;
        }
        pattern = Some(t.clone());
        pattern_idx = i;
        break;
    }
    let pattern = pattern?;
    let paths: Vec<String> = tokens[pattern_idx + 1..]
        .iter()
        .filter(|t| !t.starts_with('-'))
        .cloned()
        .collect();
    Some((pattern, paths, unsupported))
}

/// Shared equivalence predicate: can a grep-style search be transparently
/// replaced by `pixel search`? Returns the pixel command (root already
/// interpolated) if equivalent, or None if it can't be expressed. pixel
/// search is regex-based, so any pattern is expressible; only
/// output-modifying flags we can't honor fall through.
///
/// `--include`/`--exclude`/`--glob`/`--type` are file-filter flags that
/// `pixel search` doesn't support yet. We rewrite anyway and DROP them —
/// `pixel search` searches all code files (a superset of `--include`), and
/// the downstream pipeline (`| grep -v ...`) usually filters the rest.
/// This is a deliberate superset rewrite: more results, but never fewer,
/// and the agent can refine.
fn search_can_replace(pattern: &str, flags: &[String], root: &str) -> Option<String> {
    // Flags that change OUTPUT semantics in ways we can't represent.
    // File-filter flags (--include/--exclude/--glob/--type) are NOT here —
    // we drop them and search a superset.
    let unsupported_flags = [
        "-l", "--files-with-matches", "-c", "--count", "-v", "--invert",
        "-o", "--only-matching",
        "-m", "--max-count",
    ];
    if flags.iter().any(|f| unsupported_flags.contains(&f.as_str())) {
        return None;
    }
    let escaped = pattern.replace('\'', "'\\''");
    Some(format!(
        "pixel search '{}' {} --context 5",
        escaped,
        shell_quote(root)
    ))
}

/// Rewrite `rg PATTERN` / `grep PATTERN` → `pixel search PATTERN --context 5`
///
/// A single explicit path argument is preserved as the pixel search scope,
/// but only when it actually exists (file or directory) and lives inside
/// the indexed repo — rewriting a grep of `/etc/hosts` (or a typo'd path)
/// into a pixel search would silently change semantics. Multiple paths
/// can't be expressed as one pixel root, so they fall through unrewritten.
fn try_rewrite_grep(cmd: &str, cwd: &Path, root: &Path) -> Option<String> {
    // Strip trailing redirects (2>/dev/null, >file, <file) — they don't
    // change the search semantics, just I/O. The rewritten pixel command
    // doesn't need them (pixel search doesn't write to stderr in a way
    // that needs suppressing). Keep the redirect in the output so the
    // agent's intent is preserved.
    let (cmd_clean, redirect_suffix) = strip_redirects(cmd);
    let (pattern, paths, unsupported) = parse_grep(&cmd_clean)?;
    let scope = match paths.len() {
        0 => root.display().to_string(),
        1 => {
            let token = paths.into_iter().next().unwrap();
            let resolved = resolve(&token, cwd)?;
            if !resolved.is_file() && !resolved.is_dir() {
                return None;
            }
            let canon_root = canonical(root);
            if resolved != canon_root && !resolved.starts_with(&canon_root) {
                return None;
            }
            token
        }
        _ => return None,
    };
    let rewritten = search_can_replace(&pattern, &unsupported, &scope)?;
    if redirect_suffix.is_empty() {
        Some(rewritten)
    } else {
        Some(format!("{rewritten} {redirect_suffix}"))
    }
}

/// Strip trailing I/O redirects from a command segment. Returns (clean_cmd,
/// redirect_suffix). Handles `2>/dev/null`, `>file`, `2>file`, `<file`,
/// `&>file`, `1>file`. Only strips from the end — redirects in the middle
/// of a pipeline are handled by the pipe splitter before this runs.
fn strip_redirects(cmd: &str) -> (String, String) {
    let tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return (cmd.to_string(), String::new());
    }
    // Scan from the end for redirect tokens. A redirect token is one that
    // starts with a digit followed by `>`, or starts with `>`, `<`, or `&>`.
    // The token may be attached to the filename (e.g. `2>/dev/null`) or
    // separate (e.g. `2>` `/dev/null`).
    let mut redirect_start = tokens.len();
    let mut i = tokens.len();
    while i > 0 {
        i -= 1;
        let t = &tokens[i];
        // `2>/dev/null` or `>file` or `&>file` — single token with redirect+target
        if t.starts_with("2>") || t.starts_with("1>") || t.starts_with("&>")
            || t.starts_with('>') || t.starts_with('<')
        {
            redirect_start = i;
            continue;
        }
        // `2>` or `>` or `<` as a separate token — consumes the next token as filename
        if (t == "2>" || t == "1>" || t == "&>" || t == ">" || t == "<")
            && i + 1 < tokens.len()
        {
            redirect_start = i;
            continue;
        }
        // Non-redirect token — stop scanning
        break;
    }
    if redirect_start == tokens.len() {
        return (cmd.to_string(), String::new());
    }
    let clean = tokens[..redirect_start].join(" ");
    let redirect = tokens[redirect_start..].join(" ");
    (clean, redirect)
}

/// Rewrite `git log` archaeology to `pixel excavate` — but ONLY when the
/// pixel command is an exact equivalent. The original command must carry
/// nothing but ONE search term (`-S <term>` / `-Sterm` / `-G <term>` /
/// `-Gterm` / `--grep=<term>`) and optionally ONE pathspec after `--`.
/// Anything the rewrite can't represent — `--author`, `-n`/counts, rev
/// ranges, display flags, bare revs, multiple pathspecs — falls through to
/// the original command unchanged (fail open: a non-equivalent substitute
/// is worse than no guard).
fn try_rewrite_git_archaeology(cmd: &str, root: &str) -> Option<String> {
    let tokens = simple_tokenize(cmd);
    if tokens.len() < 3 || tokens[0] != "git" || tokens[1] != "log" {
        return None;
    }
    let mut phrase: Option<String> = None;
    let mut pathspecs: Vec<String> = Vec::new();
    let mut after_dashdash = false;
    let mut i = 2;
    while i < tokens.len() {
        let t = &tokens[i];
        if after_dashdash {
            pathspecs.push(t.clone());
            i += 1;
            continue;
        }
        if t == "--" {
            after_dashdash = true;
            i += 1;
            continue;
        }
        if let Some(p) = t.strip_prefix("--grep=") {
            if phrase.is_some() || p.is_empty() {
                return None;
            }
            phrase = Some(p.to_string());
            i += 1;
            continue;
        }
        if t == "-S" || t == "-G" {
            if phrase.is_some() {
                return None;
            }
            phrase = Some(tokens.get(i + 1)?.clone());
            i += 2;
            continue;
        }
        if let Some(p) = t.strip_prefix("-S").or_else(|| t.strip_prefix("-G")) {
            if phrase.is_some() || p.is_empty() {
                return None;
            }
            phrase = Some(p.to_string());
            i += 1;
            continue;
        }
        // Any other flag, rev, or range makes the rewrite non-equivalent.
        return None;
    }
    let phrase = phrase?;
    if pathspecs.len() > 1 {
        return None;
    }
    let escaped = phrase.replace('\'', "'\\''");
    let mut out = format!("pixel excavate --phrase '{}' {}", escaped, shell_quote(root));
    if let Some(p) = pathspecs.first() {
        out.push_str(&format!(" --file {}", shell_quote(p)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique scratch dir (with a `src/` subdir) acting as the
    /// indexed repo root for path-validation tests. Returns the
    /// canonicalized root so `starts_with` comparisons are stable on
    /// platforms where the temp dir is a symlink (macOS).
    fn scratch_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-guard-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        canonical(&root)
    }

    #[test]
    fn rewrite_rg_simple_pattern() {
        let cmd = "rg GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' /repo --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_grep_simple_pattern() {
        let repo = scratch_repo("dot-path");
        let cmd = "grep -rn GUARD_MATCHER .";
        let rewritten = try_rewrite_grep(cmd, &repo, &repo);
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' . --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_regex_pattern() {
        // pixel search is regex-based, so regex patterns are expressible.
        let cmd = "rg \"foo.*bar\"";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo.*bar' /repo --context 5".to_string())
        );
    }

    #[test]
    fn no_rewrite_unsupported_flags() {
        let cmd = "rg -l GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert!(rewritten.is_none(), "-l flag should not be rewritten");
    }

    #[test]
    fn no_rewrite_non_grep() {
        let cmd = "ls -la";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert!(rewritten.is_none());
    }

    #[test]
    fn rewrite_git_log_grep() {
        let cmd = "git log --grep=register_mcp";
        let rewritten = try_rewrite_git_archaeology(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel excavate --phrase 'register_mcp' /repo".to_string())
        );
    }

    #[test]
    fn no_rewrite_git_log_without_search() {
        let cmd = "git log --oneline -10";
        let rewritten = try_rewrite_git_archaeology(cmd, "/repo");
        assert!(rewritten.is_none(), "plain git log should not be rewritten");
    }

    #[test]
    fn rewrite_git_log_s_with_single_pathspec() {
        let rewritten = try_rewrite_git_archaeology("git log -S term -- src/", "/repo");
        assert_eq!(
            rewritten,
            Some("pixel excavate --phrase 'term' /repo --file src/".to_string())
        );
        // Bare -S with no pathspec keeps the plain form.
        assert_eq!(
            try_rewrite_git_archaeology("git log -S term", "/repo"),
            Some("pixel excavate --phrase 'term' /repo".to_string())
        );
        // Attached form -Sterm.
        assert_eq!(
            try_rewrite_git_archaeology("git log -Sterm", "/repo"),
            Some("pixel excavate --phrase 'term' /repo".to_string())
        );
    }

    #[test]
    fn no_rewrite_git_log_unrepresentable() {
        // Anything the excavate rewrite can't represent must fall through
        // to the original command (fail open), never a lossy substitute.
        for cmd in [
            "git log -S term --author=bob",
            "git log -S term -n 5",
            "git log -S term main..dev",
            "git log -S term v1.0",
            "git log -S term --oneline",
            "git log -S term -- src/ lib/",
            "git log -S term src/",
            "git log -S term -G other",
        ] {
            assert!(
                try_rewrite_git_archaeology(cmd, "/repo").is_none(),
                "`{cmd}` is not exactly representable and must not be rewritten"
            );
        }
    }

    #[test]
    fn advisory_json_is_non_blocking() {
        let v = advisory_json("note text");
        assert_eq!(v["systemMessage"], "note text");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "note text");
        assert!(
            v["hookSpecificOutput"].get("permissionDecision").is_none(),
            "advisory must not carry a permissionDecision (neither deny nor auto-allow)"
        );
        assert!(v.get("decision").is_none());
    }

    #[test]
    fn scoping_outside_manifest_is_advisory_not_deny() {
        let repo = scratch_repo("advisory-scope");
        let a = repo.join("src").join("a.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &c] {
            std::fs::write(f, "x").unwrap();
        }
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "t", "task": "the task", "created_unix": now_unix(),
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).unwrap();
        assert!(!allowed(&c, &m), "c.rs is outside the manifest");
        let msg = scoping_advisory_lines(&c, &m).join("\n");
        assert!(msg.contains("advisory"), "must be phrased as advisory: {msg}");
        assert!(msg.contains("src/c.rs"), "must name the file: {msg}");
        assert!(msg.contains("pixel targets"), "must suggest re-scoping: {msg}");
        assert!(!msg.contains("BLOCKED"), "must not read as a deny: {msg}");
        assert!(!msg.contains("PIXEL_TARGETS_GUARD"), "no bypass ad: {msg}");
    }

    #[test]
    fn mandate_and_index_advisories_are_non_blocking_text() {
        let repo = scratch_repo("advisory-mandate");
        let f = repo.join("src").join("a.rs");
        std::fs::write(&f, "x").unwrap();
        let msg = mandate_advisory_lines(&f, &repo).join("\n");
        assert!(msg.contains("advisory") && !msg.contains("BLOCKED"), "{msg}");
        assert!(msg.contains("pixel targets"), "{msg}");
        let msg = expired_manifest_advisory_lines(&repo).join("\n");
        assert!(msg.contains("expired") && !msg.contains("BLOCKED"), "{msg}");
        assert!(!msg.contains("PIXEL_TARGETS_GUARD"), "{msg}");
    }

    #[test]
    fn manifest_all_expired_reports_expired_state() {
        let repo = scratch_repo("expired-state");
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now_unix() - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        assert!(matches!(load_manifest_state(&repo), ManifestState::Expired));
        let missing = scratch_repo("expired-state-missing");
        assert!(matches!(load_manifest_state(&missing), ManifestState::Absent));
    }

    #[test]
    fn git_pull_denied_with_suggestion_never_rewritten() {
        // `git pull` must be a DENY with a `pixel reconcile` suggestion —
        // never a transparent rewrite, and never a suggestion containing
        // any --push flag (a push the original command didn't have).
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git pull", Some(repo)).expect("git pull must be denied");
        let msg = lines.join("\n");
        assert!(msg.contains("BLOCKED"), "must be a deny: {msg}");
        assert!(
            msg.contains("pixel reconcile /repo --strategy rebase-if-clean"),
            "must suggest reconcile: {msg}"
        );
        assert!(!msg.contains("--push"), "must never suggest --push: {msg}");
        // And the rewrite path must not touch it either.
        assert!(try_rewrite_bash("git pull", repo).is_none());
        assert!(try_rewrite_bash("git pull upstream main", repo).is_none());
    }

    #[test]
    fn git_pull_with_args_denied() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git pull upstream main", Some(repo)).is_some());
        assert!(bash_deny_lines("git pull --rebase origin main", Some(repo)).is_some());
    }

    #[test]
    fn no_deny_git_status() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git status", Some(repo)).is_none());
        assert!(try_rewrite_bash("git status", Path::new("/tmp")).is_none());
    }

    #[test]
    fn substituted_destructive_command_still_denied() {
        // The substitution bail must NOT let destructive commands through:
        // denies run before (and independent of) the conservative skip.
        let repo = Path::new("/repo");
        assert!(
            bash_deny_lines("git reset --hard $(git rev-parse HEAD~1)", Some(repo)).is_some(),
            "substitution must not bypass the destructive deny"
        );
        assert!(
            bash_deny_lines("git clean -fd `git rev-parse --show-toplevel`", Some(repo)).is_some()
        );
    }

    #[test]
    fn destructive_set_expanded() {
        let repo = Path::new("/repo");
        let denied = [
            "git reset --hard",
            "git reset --keep HEAD~2",
            "git clean -f",
            "git clean -fd",
            "git clean -fdx",
            "git clean -df",
            "git clean --force",
            "git checkout -f main",
            "git checkout --force main",
            "git checkout HEAD~1 -- src/lib.rs",
            "git restore --source HEAD~1 src/lib.rs",
            "git restore --source=HEAD~1 src/lib.rs",
            "git stash drop",
            "git stash clear",
            "git branch -D feature",
            "git push --force",
            "git push -f origin main",
        ];
        for cmd in denied {
            assert!(
                bash_deny_lines(cmd, Some(repo)).is_some(),
                "`{cmd}` must be denied"
            );
        }
        let allowed = [
            "git push --force-with-lease",
            "git push --force-with-lease=main origin main",
            "git push --force-if-includes --force-with-lease",
            "git push origin main",
            "git clean -n",
            "git checkout main",
            "git checkout -b feature",
            "git stash",
            "git stash list",
            "git stash pop",
            "git branch -d merged",
            "git branch --list",
            "git reset --soft HEAD~1",
            "git restore --staged src/lib.rs",
        ];
        for cmd in allowed {
            assert!(
                bash_deny_lines(cmd, Some(repo)).is_none(),
                "`{cmd}` must be allowed"
            );
        }
    }

    #[test]
    fn destructive_deny_robust_to_flag_order_and_segments() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git -C /repo reset --hard", Some(repo)).is_some());
        assert!(bash_deny_lines("git clean -d -f", Some(repo)).is_some());
        assert!(
            bash_deny_lines("git status && git reset --hard HEAD~1", Some(repo)).is_some(),
            "destructive segment in a compound command must be denied"
        );
    }

    #[test]
    fn quoted_destructive_text_not_denied() {
        // A destructive command mentioned inside a quoted argument is data,
        // not an executed command — the tokenizer folds it into one token.
        let repo = Path::new("/repo");
        assert!(
            bash_deny_lines("git commit -m 'do not git reset --hard here'", Some(repo)).is_none()
        );
    }

    #[test]
    fn no_deny_outside_indexed_repo() {
        assert!(bash_deny_lines("git reset --hard", None).is_none());
    }

    #[test]
    fn deny_messages_never_advertise_bypass() {
        let repo = Path::new("/repo");
        for cmd in [
            "git pull",
            "git reset --hard",
            "git clean -fd",
            "git push --force",
            "git stash drop",
        ] {
            let msg = bash_deny_lines(cmd, Some(repo)).unwrap().join("\n");
            assert!(
                !msg.contains("PIXEL_TARGETS_GUARD"),
                "deny for `{cmd}` must not advertise the kill switch: {msg}"
            );
        }
        let grep_msg = grep_deny_lines("foo", "pixel search 'foo' /repo --context 5", None).join("\n");
        assert!(!grep_msg.contains("PIXEL_TARGETS_GUARD"));
    }

    #[test]
    fn is_grep_tool_detects_pattern() {
        let mut input = serde_json::Map::new();
        input.insert("pattern".to_string(), Value::String("foo".to_string()));
        assert!(is_grep_tool("Grep", &input));
        assert!(!is_grep_tool("Bash", &input));
    }

    #[test]
    fn is_grep_tool_no_pattern_field() {
        let input = serde_json::Map::new();
        assert!(!is_grep_tool("Grep", &input));
    }

    #[test]
    fn rewrite_first_grep_segment_in_pipeline() {
        // Pipelines: the first grep/rg segment is rewritten to pixel search,
        // the rest of the pipe is preserved. This is the common agent pattern:
        // `grep -rln "pattern" ... | grep -v node_modules | sort | wc -l`
        let rewritten = try_rewrite_bash("rg foo | head -5", Path::new("/tmp"));
        assert!(rewritten.is_some(), "first grep segment in a pipeline should be rewritten");
        let cmd = rewritten.unwrap();
        assert!(cmd.starts_with("pixel search 'foo'"), "cmd was: {cmd}");
        assert!(cmd.contains("| head -5"), "rest of pipe must be preserved, cmd was: {cmd}");
    }

    #[test]
    fn pipeline_with_non_grep_first_segment_not_rewritten() {
        // If the first segment isn't grep/rg, don't touch the pipeline.
        let rewritten = try_rewrite_bash("cat foo.txt | grep bar", Path::new("/tmp"));
        assert!(rewritten.is_none(), "non-grep first segment must not be rewritten");
    }

    #[test]
    fn logical_or_not_treated_as_pipe() {
        // `||` is logical OR, not a pipe — must not be split.
        let rewritten = try_rewrite_bash("rg foo || echo failed", Path::new("/tmp"));
        assert!(rewritten.is_none(), "|| must not be treated as a pipe");
    }

    #[test]
    fn reject_control_flow_rewrite() {
        assert!(try_rewrite_bash("rg foo && echo hi", Path::new("/tmp")).is_none());
        assert!(try_rewrite_bash("rg foo; echo hi", Path::new("/tmp")).is_none());
        assert!(try_rewrite_bash("rg foo > out.txt", Path::new("/tmp")).is_none());
    }

    #[test]
    fn value_flag_skips_pattern() {
        // -A 5 consumes "5"; the pattern is "foo", not "5".
        let rewritten = try_rewrite_grep("grep -A 5 foo", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn regexp_equals_pattern() {
        let rewritten = try_rewrite_grep("grep --regexp=foo", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn scope_flag_not_rewritten() {
        // -m changes the match count; pixel search can't honor it → no rewrite.
        let repo = Path::new("/repo");
        assert!(try_rewrite_grep("grep -m 5 foo", repo, repo).is_none());
    }

    #[test]
    fn file_filter_flags_rewritten_as_superset() {
        // --include/--glob/--type are file-filter flags that pixel search
        // doesn't support yet. We rewrite anyway and DROP them — pixel search
        // searches all code files (a superset), and downstream pipeline
        // filters handle the rest. The pattern must be correctly identified.
        let repo = Path::new("/repo");
        let rewritten = try_rewrite_grep("grep --include=*.rs foo", repo, repo);
        assert!(rewritten.is_some(), "--include should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo");

        let rewritten = try_rewrite_grep("grep --glob '*.rs' foo", repo, repo);
        assert!(rewritten.is_some(), "--glob should be rewritten as superset");

        // `rg --type rust foo` must not misparse "rust" as the pattern.
        let rewritten = try_rewrite_grep("rg --type rust foo", repo, repo);
        assert!(rewritten.is_some(), "--type should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo, not rust");

        let rewritten = try_rewrite_grep("rg -t rust foo", repo, repo);
        assert!(rewritten.is_some(), "-t should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo, not rust");
    }

    #[test]
    fn preserves_path_scope() {
        let repo = scratch_repo("path-scope");
        let rewritten = try_rewrite_grep("rg foo src/", &repo, &repo);
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' src/ --context 5".to_string())
        );
    }

    #[test]
    fn nonexistent_path_not_rewritten() {
        let repo = scratch_repo("no-such-path");
        assert!(
            try_rewrite_grep("rg foo no/such/dir", &repo, &repo).is_none(),
            "a path that doesn't exist must not be silently rescoped"
        );
    }

    #[test]
    fn path_outside_repo_not_rewritten() {
        let repo = scratch_repo("outside");
        let outside = std::env::temp_dir();
        let cmd = format!("rg foo {}", outside.display());
        assert!(
            try_rewrite_grep(&cmd, &repo, &repo).is_none(),
            "a path outside the indexed repo must not be rewritten"
        );
    }

    #[test]
    fn multiple_paths_not_rewritten() {
        let repo = scratch_repo("multi-path");
        let rewritten = try_rewrite_grep("rg foo src/ lib/", &repo, &repo);
        assert!(rewritten.is_none(), "multiple roots can't be expressed");
    }

    #[test]
    fn quotes_root_with_space() {
        let rewritten = try_rewrite_grep("rg foo", Path::new("/my repo"), Path::new("/my repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' '/my repo' --context 5".to_string())
        );
    }

    #[test]
    fn accepts_before_tool_event() {
        assert!(is_guard_event("PreToolUse"));
        assert!(is_guard_event("BeforeTool"));
        assert!(!is_guard_event("PostToolUse"));
    }

    #[test]
    fn scoping_before_rewrite_destructive_not_rewritten() {
        // check_bash blocks destructive git commands; the rewrite path must
        // never turn one into a pixel command that bypasses that block.
        let rewritten = try_rewrite_bash("git reset --hard HEAD", Path::new("/tmp"));
        assert!(rewritten.is_none());
    }

    #[test]
    fn scoping_sees_grep_file_before_rewrite() {
        // Ordering guarantee: in run(), check_bash (which applies the
        // manifest scoping via single_reader_target) executes BEFORE any
        // rewrite attempt. This test proves the scoping detector still
        // extracts the file from exactly the kind of grep command the
        // rewriter would otherwise transform — so a manifest-blocked file
        // read via grep is blocked by scoping_block, never rewritten.
        let repo = scratch_repo("scope-order");
        let file = repo.join("src").join("secret.rs");
        std::fs::write(&file, "x").unwrap();
        let cmd = format!("grep foo {}", file.display());
        let detected = single_reader_target(&cmd, &repo);
        assert_eq!(detected, Some(canonical(&file)));
    }

    fn now_unix() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Write `text` as `<root>/.pixel/targets.json`.
    fn write_manifest(root: &Path, text: &str) {
        let dir = root.join(".pixel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("targets.json"), text).unwrap();
    }

    #[test]
    fn manifest_v2_union_allows_file_from_either_task() {
        let repo = scratch_repo("v2-union");
        let a = repo.join("src").join("a.rs");
        let b = repo.join("src").join("b.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &b, &c] {
            std::fs::write(f, "x").unwrap();
        }
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "aaa", "task": "task A", "created_unix": now,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                    {"id": "bbb", "task": "task B", "created_unix": now,
                     "targets": [{"path": "src/b.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("v2 manifest must load");
        assert_eq!(m.tasks.len(), 2);
        assert!(allowed(&a, &m), "file in task A must be allowed");
        assert!(
            allowed(&b, &m),
            "file listed only in task B must be allowed while task A is also active"
        );
        assert!(!allowed(&c, &m), "file in no task must be blocked");
    }

    #[test]
    fn manifest_v2_expired_task_dropped() {
        let repo = scratch_repo("v2-expiry");
        let a = repo.join("src").join("a.rs");
        let b = repo.join("src").join("b.rs");
        for f in [&a, &b] {
            std::fs::write(f, "x").unwrap();
        }
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                    {"id": "new", "task": "fresh", "created_unix": now,
                     "targets": [{"path": "src/b.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("fresh task keeps manifest alive");
        assert_eq!(m.tasks.len(), 1, "expired task must be dropped");
        assert!(!allowed(&a, &m), "expired task's file must not be allowed");
        assert!(allowed(&b, &m));
    }

    #[test]
    fn manifest_v2_all_expired_is_no_manifest() {
        let repo = scratch_repo("v2-all-expired");
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        assert!(load_manifest(&repo).is_none());
    }

    #[test]
    fn manifest_legacy_shape_still_read() {
        let repo = scratch_repo("legacy-shape");
        let a = repo.join("src").join("a.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &c] {
            std::fs::write(f, "x").unwrap();
        }
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 1,
                "task": "legacy task",
                "created_unix": now_unix(),
                "files": [{"path": "src/a.rs", "tier": "P0"}],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("legacy manifest must load");
        assert_eq!(m.tasks.len(), 1);
        assert_eq!(m.tasks[0].task, "legacy task");
        assert!(allowed(&a, &m));
        assert!(!allowed(&c, &m));
    }

    /// Write an executable fake `pixel` that prints canned search output.
    #[cfg(unix)]
    fn fake_search_exe(name: &str, script_body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "pixel-guard-fake-{}-{}",
            name,
            std::process::id()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    #[cfg(unix)]
    fn deny_with_answer_contains_search_result_lines() {
        // The fake child emits realistic pixel-search hit lines; the deny
        // message must carry them inline.
        let exe = fake_search_exe(
            "hits",
            "echo 'src/guard.rs:133: if idx_root.is_some() && is_grep_tool(tool, &tool_input) {'\n\
             echo 'src/guard.rs:140: grep_redirect(&pattern, &cwd, &tool_input);'",
        );
        let out = run_search_child(&exe, "grep_redirect", "/repo", Duration::from_secs(5))
            .expect("successful child with output must yield Some");
        let lines = grep_deny_lines(
            "grep_redirect",
            "pixel search 'grep_redirect' /repo --context 5",
            Some(&out),
        );
        let msg = lines.join("\n");
        assert!(msg.contains("BLOCKED Grep"), "must still be a deny: {msg}");
        assert!(
            msg.contains("src/guard.rs:133") && msg.contains("is_grep_tool"),
            "actual search-result lines must be inline: {msg}"
        );
        assert!(
            msg.contains("pixel search 'grep_redirect' /repo --context 5"),
            "follow-up command must be present: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn deny_falls_back_when_search_fails() {
        // Non-zero exit → run_search_child yields None → suggestion-only.
        let exe = fake_search_exe("fail", "exit 3");
        let out = run_search_child(&exe, "foo", "/repo", Duration::from_secs(5));
        assert!(out.is_none(), "failing child must yield None");
        let lines = grep_deny_lines("foo", "pixel search 'foo' /repo --context 5", None);
        let msg = lines.join("\n");
        assert!(msg.contains("Run this via Bash: pixel search 'foo' /repo --context 5"));
        assert!(
            !msg.contains("PIXEL_TARGETS_GUARD"),
            "the kill switch must not be advertised to the model: {msg}"
        );
    }

    #[test]
    fn search_child_spawn_failure_is_none() {
        let out = run_search_child(
            Path::new("/no/such/binary-pixel-guard-test"),
            "foo",
            "/repo",
            Duration::from_secs(1),
        );
        assert!(out.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn search_child_times_out() {
        let exe = fake_search_exe("slow", "sleep 30\necho late");
        let started = Instant::now();
        let out = run_search_child(&exe, "foo", "/repo", Duration::from_millis(200));
        assert!(out.is_none(), "timed-out child must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must not wait for the child's full sleep"
        );
    }

    #[test]
    fn truncate_results_caps_lines() {
        let long: String = (0..200)
            .map(|i| format!("line {i}\n"))
            .collect();
        let out = truncate_results(&long);
        assert!(out.lines().count() <= SEARCH_ANSWER_MAX_LINES + 1);
        assert!(out.contains("results truncated"));
        let short = truncate_results("just one line");
        assert_eq!(short, "just one line");
    }

    #[test]
    fn truncate_results_caps_bytes() {
        let wide = format!("{}\nnext", "x".repeat(SEARCH_ANSWER_MAX_BYTES * 2));
        let out = truncate_results(&wide);
        assert!(out.len() < SEARCH_ANSWER_MAX_BYTES + 200);
        assert!(out.contains("results truncated"));
    }

    #[test]
    fn grep_redirect_allows_unsupported_fields() {
        // A Grep tool call carrying fields pixel search can't express
        // (glob/type/output_mode) must be allowed through, not denied with
        // a non-equivalent suggestion. grep_redirect returns false (allow)
        // instead of calling block() (which would exit the process).
        for field in ["glob", "type", "output_mode"] {
            let mut input = serde_json::Map::new();
            input.insert("pattern".to_string(), Value::String("foo".to_string()));
            input.insert(field.to_string(), Value::String("x".to_string()));
            assert!(
                !grep_redirect("foo", Path::new("/tmp"), &input),
                "Grep with `{field}` field must be allowed through"
            );
        }
    }
}
