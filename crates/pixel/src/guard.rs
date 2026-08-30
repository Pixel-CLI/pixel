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
//! 4. GLOB — Glob tool calls are deliberately left un-denied: they only
//!    enumerate paths, and the Read/Edit of any result is itself guarded by
//!    the scoping rules above. Blocking enumeration would be pure noise.
//!
//! Blocks by exiting 2 with a corrective message on stderr (the exit code
//! Claude Code's hook protocol treats as "deny, feed stderr to the model").
//! Fails open (exit 0) on any parse error or unexpected shape — a guard
//! that crashes or wedges the session is worse than a guard that misses a
//! case. Kill switch: `PIXEL_TARGETS_GUARD=0`.

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
    let manifest = manifest_root.as_deref().and_then(load_manifest);

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

/// Read the enforcement manifest, accepting BOTH shapes:
/// - v2 (multi-task): `{version: 2, tasks: [{id, task, created_unix, targets: [...]}]}`
/// - legacy (v1/singleton): `{task, created_unix, files: [...]}`
/// Expired tasks (older than the 24h TTL) are dropped individually; a
/// manifest whose tasks have all expired counts as no manifest at all.
fn load_manifest(root: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(root.join(".pixel").join("targets.json")).ok()?;
    let m: Value = serde_json::from_str(&text).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let tasks: Vec<TaskEntry> = if m.get("version").and_then(Value::as_u64) == Some(2) {
        m.get("tasks")?
            .as_array()?
            .iter()
            .filter(|t| {
                let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
                now.saturating_sub(created) <= MANIFEST_MAX_AGE_SECS
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
            return None;
        }
        vec![TaskEntry {
            task: m.get("task").and_then(Value::as_str).unwrap_or("?").to_string(),
            files: parse_manifest_files(m.get("files")?.as_array()?),
        }]
    };
    if tasks.is_empty() {
        return None;
    }
    Some(Manifest { root: root.to_path_buf(), tasks })
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

/// Truncate a task string for display (char-safe, appends an ellipsis).
fn short_task(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

fn scoping_block(abs: &Path, m: &Manifest) -> ! {
    let rel = rel_of(abs, &m.root);
    let p0: Vec<&str> = all_files(m)
        .filter(|(_, tier)| tier == "P0")
        .map(|(p, _)| p.as_str())
        .take(5)
        .collect();
    let total: usize = m.tasks.iter().map(|t| t.files.len()).sum();
    let mut lines = vec![format!(
        "BLOCKED by pixel-targets-guard: sniper targets active for {} task(s):",
        m.tasks.len()
    )];
    lines.extend(
        m.tasks
            .iter()
            .map(|t| format!("  - '{}'", short_task(&t.task, 70))),
    );
    lines.push(format!(
        "'{rel}' is not in any task's target list. Work only on listed files, P0 first:"
    ));
    lines.extend(p0.iter().map(|p| format!("  P0: {p}")));
    lines.push(format!("  ({total} file(s) total in .pixel/targets.json)"));
    lines.push(
        "If this file is genuinely needed, the task description was wrong — re-run".into(),
    );
    lines.push(
        "`pixel targets \"<refined task>\"` to add/refresh YOUR task's list (other".into(),
    );
    lines.push(
        "tasks are preserved), or `pixel targets --clear` to end ALL scoping.".into(),
    );
    lines.push("Do NOT bypass via other tools.".into());
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
            "To bypass: PIXEL_TARGETS_GUARD=0".into(),
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

    // Skip complex commands — only rewrite simple single commands.
    // Heredocs, command substitution, pipelines, and control-flow
    // operators are left alone (conservative: never guess at a pipeline).
    if trimmed.contains("<<")
        || trimmed.contains("$(")
        || trimmed.contains('`')
        || has_unquoted_meta(trimmed)
    {
        return None;
    }

    let root_dir = find_up(cwd, ".pixel").unwrap_or_else(|| cwd.to_path_buf());
    let root = root_dir.display().to_string();

    // --- rg / grep → pixel search ---
    if let Some(rewritten) = try_rewrite_grep(trimmed, cwd, &root_dir) {
        return Some(rewritten);
    }

    // --- git log / git show with search intent → pixel excavate ---
    if let Some(rewritten) = try_rewrite_git_archaeology(trimmed, &root) {
        return Some(rewritten);
    }

    // --- git fetch + merge/rebase → pixel reconcile ---
    if let Some(rewritten) = try_rewrite_git_sync(trimmed, &root) {
        return Some(rewritten);
    }

    None
}

/// Flags that consume a following value (or an attached `=value`), so they
/// must be skipped when locating the search pattern.
const VALUE_FLAGS: &[&str] = &[
    "-A", "-B", "-C", "-m", "-g", "-t", "-f", "--include", "--exclude",
    "--glob", "--type", "-d", "--max-depth",
];

/// Value-consuming flags that also change the file scope or match count in
/// ways `pixel search` can't reproduce. Their presence makes a rewrite
/// non-equivalent, so the command falls through to the original.
const SCOPE_FLAGS: &[&str] = &[
    "-m", "-g", "-t", "-f", "-d", "--include", "--exclude", "--glob",
    "--type", "--max-depth",
];

/// True if `cmd` contains a shell metacharacter outside of quotes. Used to
/// refuse rewriting pipelines and control-flow operators — never guess at a
/// compound command.
fn has_unquoted_meta(cmd: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if matches!(c, '|' | '&' | ';' | '>' | '<' | '\n') => return true,
            None => {}
        }
    }
    false
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
fn search_can_replace(pattern: &str, flags: &[String], root: &str) -> Option<String> {
    let unsupported_flags = [
        "-l", "--files-with-matches", "-c", "--count", "-v", "--invert",
        "-o", "--only-matching",
        "-m", "--max-count", "-g", "--glob", "-t", "--type", "-f",
        "--include", "--exclude", "-d", "--max-depth",
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
    let (pattern, paths, unsupported) = parse_grep(cmd)?;
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
    search_can_replace(&pattern, &unsupported, &scope)
}

/// Rewrite `git log --grep=PHRASE` / `git log -S PHRASE` / `git show` with
/// search intent → `pixel excavate --phrase PHRASE`
fn try_rewrite_git_archaeology(cmd: &str, root: &str) -> Option<String> {
    let tokens = simple_tokenize(cmd);
    if tokens.len() < 2 {
        return None;
    }
    if tokens[0] != "git" {
        return None;
    }
    let sub = tokens[1].as_str();
    match sub {
        "log" => {
            // Look for --grep=, -S, -G (search intent)
            for t in &tokens[2..] {
                if let Some(p) = t.strip_prefix("--grep=") {
                    let escaped = p.replace('\'', "'\\''");
                    return Some(format!("pixel excavate --phrase '{}' {}", escaped, shell_quote(root)));
                }
            }
            // -S <pattern> or -G <pattern>
            for (i, t) in tokens[2..].iter().enumerate() {
                if (t == "-S" || t == "-G") && i + 1 < tokens.len() - 2 {
                    let pattern = tokens[i + 3].clone();
                    let escaped = pattern.replace('\'', "'\\''");
                    return Some(format!("pixel excavate --phrase '{}' {}", escaped, shell_quote(root)));
                }
            }
            None
        }
        _ => None,
    }
}

/// Rewrite `git fetch && git merge` / `git pull` / `git fetch && git rebase`
/// → `pixel reconcile`
fn try_rewrite_git_sync(cmd: &str, root: &str) -> Option<String> {
    let tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] != "git" {
        return None;
    }
    let sub = tokens.get(1).map(String::as_str).unwrap_or("");
    // `git pull` = fetch + merge — rewrite to reconcile
    if sub == "pull" {
        return Some(format!("pixel reconcile {} --strategy rebase-if-clean --push auto", shell_quote(root)));
    }
    // `git fetch ... && git merge/rebase ...` — detect compound
    if sub == "fetch" && cmd.contains("&&") {
        let lower = cmd.to_lowercase();
        if lower.contains("merge") || lower.contains("rebase") {
            return Some(format!("pixel reconcile {} --strategy rebase-if-clean --push auto", shell_quote(root)));
        }
    }
    None
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
    fn rewrite_git_pull() {
        let cmd = "git pull";
        let rewritten = try_rewrite_git_sync(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel reconcile /repo --strategy rebase-if-clean --push auto".to_string())
        );
    }

    #[test]
    fn no_rewrite_git_status() {
        let cmd = "git status";
        let rewritten = try_rewrite_git_sync(cmd, "/repo");
        assert!(rewritten.is_none());
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
    fn reject_pipeline_rewrite() {
        let rewritten = try_rewrite_bash("rg foo | head -5", Path::new("/tmp"));
        assert!(rewritten.is_none(), "pipelines must not be rewritten");
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
        // --include/--glob/--type/-m change the file scope or match count;
        // pixel search can't honor them, so the rewrite must fall through.
        let repo = Path::new("/repo");
        assert!(try_rewrite_grep("grep --include=*.rs foo", repo, repo).is_none());
        assert!(try_rewrite_grep("grep --glob '*.rs' foo", repo, repo).is_none());
        assert!(try_rewrite_grep("grep -m 5 foo", repo, repo).is_none());
    }

    #[test]
    fn rg_type_flag_not_rewritten() {
        // `rg --type rust foo` must not misparse "rust" as the pattern, and
        // --type is a scope flag pixel search can't honor → no rewrite.
        let repo = Path::new("/repo");
        assert!(try_rewrite_grep("rg --type rust foo", repo, repo).is_none());
        assert!(try_rewrite_grep("rg -t rust foo", repo, repo).is_none());
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
        assert!(msg.contains("PIXEL_TARGETS_GUARD=0"));
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
