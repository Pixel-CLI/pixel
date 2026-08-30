//! `pixel hook guard` — mechanical enforcement of the sniper-targets
//! contract, ported from the original working `gitpixel-targets-guard`
//! Python hook (kept as `~/.claude/hooks/gitpixel-targets-guard.pixel-bak.*`
//! on this machine). The prior Rust implementation of this hook only
//! printed the manifest and never blocked anything — this replaces that
//! with real PreToolUse enforcement.
//!
//! Contract (unchanged from the original):
//! 1. SCOPING — while `<repo>/.pixel/targets.json` is active (younger than
//!    24h), reads/greps/edits of repo files OUTSIDE the target list are
//!    blocked with a corrective message.
//! 2. MANDATE — in a pixel-indexed repo (a `.pixel` dir exists) with NO
//!    active manifest, edits to *existing* files are blocked: an
//!    implementation task must start with `pixel targets "<task>"`.
//! 3. RESCUE — destructive history restores (`git reset --hard`,
//!    `git checkout <ref> -- <path>`, `git restore --source`) are blocked:
//!    use `pixel rescue` instead.
//!
//! Blocks by exiting 2 with a corrective message on stderr (the exit code
//! Claude Code's hook protocol treats as "deny, feed stderr to the model").
//! Fails open (exit 0) on any parse error or unexpected shape — a guard
//! that crashes or wedges the session is worse than a guard that misses a
//! case. Kill switch: `PIXEL_TARGETS_GUARD=0`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const MANIFEST_MAX_AGE_SECS: u64 = 24 * 3600;
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

struct Manifest {
    root: PathBuf,
    task: String,
    files: Vec<(String, String)>, // (path, tier)
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
    if event != "PreToolUse" {
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
    let manifest = manifest_root.as_deref().and_then(load_manifest);

    if tool == "Bash" || tool == "exec" || tool == "bash" || tool == "run_shell_command" || tool == "execute" {
        let cmd = tool_input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        check_bash(cmd, &cwd, idx_root.as_deref(), manifest.as_ref());
        std::process::exit(0);
    }

    match tool {
        "Read" | "Grep" | "Glob"
        | "read" | "grep" | "find_file_by_name" | "glob" | "notebook_read"
        | "read_file" | "search" => {
            if let Some(m) = &manifest {
                let p = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));
                if !allowed(&p, m) {
                    scoping_block(&p, m);
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
                    scoping_block(&p, m);
                }
                std::process::exit(0);
            }
            // MANDATE — indexed repo, no manifest: edits to existing files
            // require scoping first.
            if let Some(root) = &idx_root {
                if exists && !is_exempt(&p, root) {
                    mandate_block(&p, root);
                }
            } else if exists {
                // Unindexed git repo: suggest indexing so the guard can
                // enforce scoping. Only fires for edits to existing files
                // in repos that have .git/ but no .pixel/.
                if let Some(git_root) = find_up(&anchor, ".git") {
                    suggest_index_block(&git_root);
                }
            }
        }
        _ => {}
    }
    std::process::exit(0);
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

fn load_manifest(root: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(root.join(".pixel").join("targets.json")).ok()?;
    let m: Value = serde_json::from_str(&text).ok()?;
    let files_raw = m.get("files")?.as_array()?;
    let created_unix = m.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(created_unix) > MANIFEST_MAX_AGE_SECS {
        return None;
    }
    let task = m.get("task").and_then(Value::as_str).unwrap_or("?").to_string();
    let files = files_raw
        .iter()
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?.to_string();
            let tier = f.get("tier").and_then(Value::as_str).unwrap_or("").to_string();
            Some((path, tier))
        })
        .collect();
    Some(Manifest { root: root.to_path_buf(), task, files })
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
    let target_paths: HashSet<&str> = m.files.iter().map(|(p, _)| p.as_str()).collect();
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

fn scoping_block(abs: &Path, m: &Manifest) -> ! {
    let rel = rel_of(abs, &m.root);
    let p0: Vec<&str> = m
        .files
        .iter()
        .filter(|(_, tier)| tier == "P0")
        .map(|(p, _)| p.as_str())
        .take(5)
        .collect();
    let mut lines = vec![
        format!(
            "BLOCKED by pixel-targets-guard: sniper targets active for task '{}'.",
            m.task
        ),
        format!("'{rel}' is not in the target list. Work only on listed files, P0 first:"),
    ];
    lines.extend(p0.iter().map(|p| format!("  P0: {p}")));
    lines.push(format!("  ({} file(s) total in .pixel/targets.json)", m.files.len()));
    lines.push(
        "If this file is genuinely needed, the task description was wrong — re-run".into(),
    );
    lines.push(
        "`pixel targets \"<refined task>\"` to regenerate the list, or".into(),
    );
    lines.push(
        "`pixel targets --clear` to end scoping. Do NOT bypass via other tools.".into(),
    );
    block(&lines);
}

fn mandate_block(abs: &Path, idx_root: &Path) -> ! {
    let rel = rel_of(abs, idx_root);
    block(&[
        "BLOCKED by pixel-targets-guard: no sniper target list is active for this repo.".into(),
        "Every implementation task MUST be scoped before editing:".into(),
        "  pixel targets \"<one-line task description>\" .".into(),
        "That returns the closed P0/P1/P2 file list and activates .pixel/targets.json;".into(),
        format!("then edit only listed files (this edit: {rel})."),
        "Ending a task: pixel targets --clear".into(),
    ]);
}

/// Block edits in a git repo that hasn't been indexed by pixel yet.
/// The guard can't enforce scoping without an index — tell the agent to
/// index first, then the mandate_block path takes over on the next call.
fn suggest_index_block(git_root: &Path) -> ! {
    block(&[
        "BLOCKED by pixel-targets-guard: this is a git repo but pixel has not indexed it.".into(),
        "The guard can only enforce scoping in indexed repos.".into(),
        "Index it now (one-time, takes seconds):".into(),
        format!("  pixel index {}", git_root.display()),
        "Then scope your task before editing:".into(),
        "  pixel targets \"<one-line task description>\" .".into(),
        "To bypass for repos you don't want pixel in: PIXEL_TARGETS_GUARD=0".into(),
    ]);
}

/// Bash-command checks. Conservative by design (false negatives are
/// acceptable, false positives are not) — anything containing command
/// substitution or control-flow keywords is skipped rather than guessed at.
fn check_bash(cmd: &str, cwd: &Path, idx_root: Option<&Path>, manifest: Option<&Manifest>) {
    // A heredoc body, command substitution, or backtick expansion can
    // contain literal text that looks like a destructive git command (e.g.
    // this very guard's own commit message describing what it blocks) —
    // that text is data, not a command being executed. Bail out entirely
    // rather than risk a false positive; conservative by design (false
    // negatives are acceptable here, false positives are not).
    if cmd.contains("<<") || cmd.contains("$(") || cmd.contains('`') {
        return;
    }
    if idx_root.is_some() && cmd.contains("git") {
        if is_git_reset_hard(cmd) {
            block(&[
                "BLOCKED by pixel-targets-guard: `git reset --hard` destroys in-progress work."
                    .into(),
                "\"It was working before\" is a rescue problem — use the surgical planner:".into(),
                "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
                "  pixel rescue --apply <oid> --file <path>  # gated restore (working tree only)".into(),
                "Dirty files: add --merge (3-way, keeps your edits) or --stash-first.".into(),
            ]);
        }
        if is_git_raw_restore(cmd) {
            block(&[
                "BLOCKED by pixel-targets-guard: raw historical file restore can clobber in-progress work.".into(),
                "Use the surgical planner instead:".into(),
                "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
                "  pixel rescue --apply <oid> --file <path> [--merge|--stash-first]".into(),
            ]);
        }
    }

    if let Some(m) = manifest {
        if let Some(first_file) = single_reader_target(cmd, cwd) {
            if !allowed(&first_file, m) {
                scoping_block(&first_file, m);
            }
        }
    }
}

fn is_git_reset_hard(cmd: &str) -> bool {
    let Some(idx) = cmd.find("reset") else { return false };
    cmd.contains("git") && cmd[idx..].contains("--hard")
}

fn is_git_raw_restore(cmd: &str) -> bool {
    if cmd.contains("checkout") && cmd.contains(" -- ") {
        return true;
    }
    if let Some(idx) = cmd.find("restore") {
        return cmd[idx..].contains("--source");
    }
    false
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
