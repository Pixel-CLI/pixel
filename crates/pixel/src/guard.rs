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
    block(&[
        "BLOCKED by pixel-guard: use pixel search instead of Grep in indexed repos.".into(),
        format!("Run this via Bash: {}", cmd),
        "pixel search returns the match + surrounding code (no follow-up Read needed).".into(),
        "To bypass: PIXEL_TARGETS_GUARD=0".into(),
    ]);
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

    let root = find_up(cwd, ".pixel")
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| ".".to_string());

    // --- rg / grep → pixel search ---
    if let Some(rewritten) = try_rewrite_grep(trimmed, &root) {
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
fn try_rewrite_grep(cmd: &str, root: &str) -> Option<String> {
    let (pattern, paths, unsupported) = parse_grep(cmd)?;
    // pixel search takes a single root; multiple path args can't be expressed.
    let scope = match paths.len() {
        0 => root.to_string(),
        1 => paths.into_iter().next().unwrap(),
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

    #[test]
    fn rewrite_rg_simple_pattern() {
        let cmd = "rg GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' /repo --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_grep_simple_pattern() {
        let cmd = "grep -rn GUARD_MATCHER .";
        let rewritten = try_rewrite_grep(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' . --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_regex_pattern() {
        // pixel search is regex-based, so regex patterns are expressible.
        let cmd = "rg \"foo.*bar\"";
        let rewritten = try_rewrite_grep(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'foo.*bar' /repo --context 5".to_string())
        );
    }

    #[test]
    fn no_rewrite_unsupported_flags() {
        let cmd = "rg -l GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, "/repo");
        assert!(rewritten.is_none(), "-l flag should not be rewritten");
    }

    #[test]
    fn no_rewrite_non_grep() {
        let cmd = "ls -la";
        let rewritten = try_rewrite_grep(cmd, "/repo");
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
        let rewritten = try_rewrite_grep("grep -A 5 foo", "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn regexp_equals_pattern() {
        let rewritten = try_rewrite_grep("grep --regexp=foo", "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn scope_flag_not_rewritten() {
        // --include/--glob/--type/-m change the file scope or match count;
        // pixel search can't honor them, so the rewrite must fall through.
        assert!(try_rewrite_grep("grep --include=*.rs foo", "/repo").is_none());
        assert!(try_rewrite_grep("grep --glob '*.rs' foo", "/repo").is_none());
        assert!(try_rewrite_grep("grep -m 5 foo", "/repo").is_none());
    }

    #[test]
    fn preserves_path_scope() {
        let rewritten = try_rewrite_grep("rg foo src/", "/repo");
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' src/ --context 5".to_string())
        );
    }

    #[test]
    fn multiple_paths_not_rewritten() {
        let rewritten = try_rewrite_grep("rg foo src/ lib/", "/repo");
        assert!(rewritten.is_none(), "multiple roots can't be expressed");
    }

    #[test]
    fn quotes_root_with_space() {
        let rewritten = try_rewrite_grep("rg foo", "/my repo");
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
}
