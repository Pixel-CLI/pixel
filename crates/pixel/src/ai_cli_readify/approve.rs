// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Auto-approval: clearing the gate an agent puts in front of its own first
//! turn.
//!
//! Three of the four agents gate a fresh workspace. Codex asks about the
//! folder's trust level and about the hooks it would run there; Claude Code
//! asks for onboarding and for the trust dialog; Antigravity and Devin read a
//! trust list and never write one. The reference this port mirrors clears the
//! first two unconditionally — it runs inside a throwaway recording sandbox
//! whose workspaces are clones made for the recording, so "trust this folder"
//! there is answered about a directory that will be deleted.
//!
//! This port keeps the mechanism and refuses the assumption. A trust write
//! outlives the run: it means "run this folder's hooks and code without
//! asking me again", and which folders get that is the user's decision, not
//! this command's. So the writes happen only under `--approve`, only for the
//! one workspace named on the command line, and the report says which of the
//! two happened rather than leaving the difference invisible.
//!
//! Two things are deliberately *not* done here, both because the reference
//! refuses them and its reasoning survives the move out of the sandbox:
//!
//! - **No hash is read back from disk.** Codex's app-server computes the hash
//!   of each hook as it stands now. A hash stored in a config file records
//!   what that hook used to be, so trusting it would approve whatever
//!   replaced it. The hash written here is the one the server just reported.
//! - **No hash is written into `config.toml` by hand.** Both Codex writes go
//!   through Codex's own `config/batchWrite` RPC, so the server applies its
//!   own rules and its own file versioning to a file it owns.

use std::{fs, path::Path, time::Duration};

use serde::Serialize;
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};

use super::Agent;
use super::config::{
    CLAUDE_ONBOARDING_FILE, codex_config, resolve_target, temp_for, unchanged_since,
};
use super::rpc::{self, MergeStrategy};

/// The Codex config key the `config/batchWrite` RPC addresses hook trust by.
const HOOKS_STATE_KEY: &str = "hooks.state";

/// The `[projects."<path>"]` key Codex records a folder's trust under, and
/// the value that means "do not ask again".
const TRUST_LEVEL_KEY: &str = "trust_level";
const TRUSTED: &str = "trusted";

/// Claude's own spellings, in `~/.claude.json`.
const ONBOARDED_FIELD: &str = "hasCompletedOnboarding";
const TRUST_FIELD: &str = "hasTrustDialogAccepted";
const PROJECTS_FIELD: &str = "projects";

/// What one agent's approval attempt did, in the terms the report prints.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Approval {
    pub(crate) agent: &'static str,
    /// True when nothing is left asking — including the case where there was
    /// nothing to answer.
    pub(crate) approved: bool,
    /// What was written, or why nothing was.
    pub(crate) detail: String,
}

impl Approval {
    fn done(agent: Agent, detail: String) -> Self {
        Self {
            agent: agent.name(),
            approved: true,
            detail,
        }
    }

    fn refused(agent: Agent, detail: String) -> Self {
        Self {
            agent: agent.name(),
            approved: false,
            detail,
        }
    }
}

/// Approve `agent`'s startup gate for `workspace`.
///
/// `timeout` bounds each app-server exchange, not the whole call: Codex needs
/// two of them, and a slow server on the first should not eat the second's
/// budget.
pub(crate) fn approve(home: &Path, agent: Agent, workspace: &Path, timeout: Duration) -> Approval {
    match agent {
        Agent::Codex => codex(home, workspace, timeout),
        Agent::Claude => claude(home, workspace),
        // The reference reads these two agents' trust state and never writes
        // it — Antigravity's `trustedWorkspaces` and Devin's
        // `trusted_workspaces.json` have no writer anywhere in it. Inventing
        // one here would be this command claiming a mechanism neither agent
        // has been shown to accept, so the report names the prompt instead.
        Agent::Antigravity | Agent::Devin => Approval::refused(
            agent,
            format!(
                "{}: no approval path — its trust state is read-only here; accept its workspace prompt by hand",
                agent.name()
            ),
        ),
    }
}

/// Codex's two gates, cleared through its own app-server.
fn codex(home: &Path, workspace: &Path, timeout: Duration) -> Approval {
    match clear_codex(home, workspace, timeout) {
        Ok(detail) => Approval::done(Agent::Codex, detail),
        Err(detail) => Approval::refused(Agent::Codex, detail),
    }
}

/// The two `codex` app-server exchanges [`clear_codex_with`] makes, behind a
/// trait so the policy over their answers can be tested without a server.
///
/// [`rpc`] already carries the skip for the parts that spawn a real `codex`
/// (`Session::spawn`, `hooks_list`, `config_batch_write`), the same way
/// [`workspace_trusted`] is split from the decision in
/// [`workspace_trusted_in`]. What was left under the spawn is the policy:
/// that an empty hook state is not written, and that a workspace already
/// recorded as trusted is not written again. Policy no test can reach is
/// policy that comes back inverted, and both of those writes outlive the run.
trait CodexRpc {
    /// The workspace's hooks, each with the hash the server computed for it.
    fn hooks(&self) -> Result<Vec<rpc::HookEntry>, String>;
    /// One `config/batchWrite` edit.
    fn write(
        &self,
        key_path: String,
        value: Value,
        merge_strategy: MergeStrategy,
    ) -> Result<(), String>;
}

/// The real server: one spawn per exchange, which is what [`rpc`] does.
struct AppServer<'a> {
    workspace: &'a Path,
    timeout: Duration,
}

impl CodexRpc for AppServer<'_> {
    // No logic of its own: each of these is the `rpc` function of the same
    // name with this struct's two fields passed on, and both of those
    // already carry `mutants::skip` because they spawn a real `codex`. A
    // mutant here is a mutant in code no test can reach, reported as a
    // survivor; the decisions worth mutating live in `clear_codex_with`,
    // which the fake below does reach.
    #[cfg_attr(test, mutants::skip)] // delegates to `rpc::hooks_list`, which spawns a real `codex`
    fn hooks(&self) -> Result<Vec<rpc::HookEntry>, String> {
        rpc::hooks_list(self.workspace, self.timeout)
    }

    #[cfg_attr(test, mutants::skip)] // delegates to `rpc::config_batch_write`, which spawns a real `codex`
    fn write(
        &self,
        key_path: String,
        value: Value,
        merge_strategy: MergeStrategy,
    ) -> Result<(), String> {
        rpc::config_batch_write(
            self.workspace,
            key_path,
            value,
            merge_strategy,
            self.timeout,
        )
    }
}

fn clear_codex(home: &Path, workspace: &Path, timeout: Duration) -> Result<String, String> {
    // One path for both halves. The trust entry is keyed by the canonical
    // path, and the app-server has to be asked about that same one: the
    // child runs with its cwd set to the workspace, and it asks about the
    // workspace it was handed, so a relative `--workspace` (the default is
    // `.`) has the server resolve one directory deeper, and a symlinked one
    // has it answer under a path the trust key does not name. Either way
    // `hooks/list` describes a workspace this command never trusts:
    // `hooks.state` goes unwritten and the run reports `approved: true` with
    // the hook review prompt still up. The Claude path below resolves both
    // of its paths for the same reason.
    let real_path = fs::canonicalize(workspace)
        .map_err(|e| format!("codex: cannot resolve {}: {e}", workspace.display()))?;
    let real = real_path.to_string_lossy().into_owned();
    clear_codex_with(
        &real,
        &codex_config(home),
        &AppServer {
            workspace: &real_path,
            timeout,
        },
    )
}

/// The decisions, over a transport a test can stand in for.
fn clear_codex_with(real: &str, config: &Path, rpc: &dyn CodexRpc) -> Result<String, String> {
    let mut written = Vec::new();

    let entries = rpc.hooks()?;
    let state = match rpc::hook_state(&entries) {
        // An enabled hook the server reported no current hash for cannot be
        // trusted by this command, and guessing at one is the whole thing the
        // RPC exists to avoid. Say so instead of writing a partial map.
        None => {
            return Err(
                "codex: an enabled hook has no current hash from the app-server — review it with /hooks"
                    .to_string(),
            )
        }
        Some(Value::Object(state)) => state,
        Some(other) => {
            return Err(format!(
                "codex: hook state came back as {other}, not an object — not written"
            ))
        }
    };
    if !state.is_empty() {
        let count = state.len();
        rpc.write(
            HOOKS_STATE_KEY.to_string(),
            Value::Object(state),
            MergeStrategy::Upsert,
        )?;
        written.push(format!(
            "{count} enabled hook(s) trusted at their current hashes"
        ));
    }

    if workspace_trusted(config, real) {
        written.push(format!("{real} was already trusted"));
    } else {
        rpc.write(
            rpc::trust_key_path(real),
            Value::String(TRUSTED.to_string()),
            MergeStrategy::Replace,
        )?;
        written.push(format!("{real} marked {TRUSTED}"));
    }

    Ok(format!("codex: {}", written.join("; ")))
}

/// Whether Codex's config already records `real` as trusted.
///
/// A file that cannot be read answers `false`: the write that follows is the
/// server's own, and it is the server's job to refuse a config it cannot
/// apply to.
fn workspace_trusted(config: &Path, real: &str) -> bool {
    fs::read_to_string(config).is_ok_and(|text| workspace_trusted_in(&text, real))
}

/// The decision, split from the read so it can be tested without a file.
fn workspace_trusted_in(text: &str, real: &str) -> bool {
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return false;
    };
    doc.get(PROJECTS_FIELD)
        .and_then(Item::as_table_like)
        .and_then(|projects| projects.get(real))
        .and_then(Item::as_table_like)
        .and_then(|project| project.get(TRUST_LEVEL_KEY))
        .and_then(Item::as_str)
        == Some(TRUSTED)
}

/// Claude's onboarding and trust dialog, recorded in `~/.claude.json`.
fn claude(home: &Path, workspace: &Path) -> Approval {
    let path = home.join(CLAUDE_ONBOARDING_FILE);
    // Canonical, as the Codex path above already is. `~/.claude.json` keys
    // `projects` on the real path, so a relative or symlinked `--workspace`
    // wrote an entry Claude Code never looks up: the write succeeded, the
    // report said trusted, and the prompt still stood. The reference
    // canonicalises for the same reason (`realpath` in main.ts).
    let workspace = match fs::canonicalize(workspace) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(e) => {
            return Approval::refused(
                Agent::Claude,
                format!("claude: cannot resolve {}: {e}", workspace.display()),
            );
        }
    };
    let (config, original) = match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => (value, text),
            Err(e) => {
                return Approval::refused(
                    Agent::Claude,
                    format!(
                        "claude: {} is not valid JSON ({e}) — not touched",
                        path.display()
                    ),
                );
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (Value::Object(Map::new()), String::new())
        }
        Err(e) => {
            return Approval::refused(
                Agent::Claude,
                format!("claude: cannot read {}: {e}", path.display()),
            );
        }
    };

    match claude_trusted(&config, &workspace) {
        Err(reason) => Approval::refused(
            Agent::Claude,
            format!("claude: {reason} in {} — not touched", path.display()),
        ),
        Ok(None) => Approval::done(
            Agent::Claude,
            format!("claude: {workspace} was already onboarded and trusted"),
        ),
        Ok(Some(next)) => match write_private(&path, &original, &next) {
            Ok(()) => Approval::done(
                Agent::Claude,
                format!(
                    "claude: recorded onboarding and workspace trust for {workspace} in {}",
                    path.display()
                ),
            ),
            Err(e) => Approval::refused(Agent::Claude, format!("claude: {e}")),
        },
    }
}

/// Whether the config's `projects` object already marks `workspace` trusted.
///
/// An absent `projects`, or an absent entry for the workspace, is `false` —
/// the flags are missing, not corrupt. A `projects` that is present but is not
/// an object, or an entry that is not, is an error: that file holds something
/// this command did not write and must not overwrite.
fn already_trusted(object: &Map<String, Value>, workspace: &str) -> Result<bool, String> {
    let Some(projects) = object.get(PROJECTS_FIELD) else {
        return Ok(false);
    };
    let projects = projects
        .as_object()
        .ok_or_else(|| format!("`{PROJECTS_FIELD}` is not an object"))?;
    let Some(entry) = projects.get(workspace) else {
        return Ok(false);
    };
    Ok(entry
        .as_object()
        .ok_or_else(|| format!("`{PROJECTS_FIELD}.{workspace}` is not an object"))?
        .get(TRUST_FIELD)
        == Some(&Value::Bool(true)))
}

/// The Claude config that records onboarding and workspace trust, or the
/// reason it cannot be produced.
///
/// `Ok(None)` means the file already records both and there is nothing to
/// write — a different outcome from `Ok(Some(_))`, and a different one again
/// from `Err`: an unreadable shape is a refusal, not a no-op, and collapsing
/// the three would report a file this command declined to touch as a file
/// that needed nothing.
///
/// Both fields are set whenever either is missing, as the reference does:
/// they are the two halves of one dialog's outcome, and an onboarding flag
/// without the trust flag leaves the prompt standing.
pub(crate) fn claude_trusted(config: &Value, workspace: &str) -> Result<Option<Value>, String> {
    let object = config
        .as_object()
        .ok_or_else(|| "the file is not a JSON object".to_string())?;
    let trusted = already_trusted(object, workspace)?;
    if object.get(ONBOARDED_FIELD) == Some(&Value::Bool(true)) && trusted {
        return Ok(None);
    }

    let mut next = config.clone();
    let object = next
        .as_object_mut()
        .expect("checked to be an object above, and cloning preserves the shape");
    object.insert(ONBOARDED_FIELD.to_string(), Value::Bool(true));
    let projects = object
        .entry(PROJECTS_FIELD.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let entry = projects
        .as_object_mut()
        .expect("checked to be an object above, and cloning preserves the shape")
        .entry(workspace.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry
        .as_object_mut()
        .expect("checked to be an object above, and cloning preserves the shape")
        .insert(TRUST_FIELD.to_string(), Value::Bool(true));
    Ok(Some(next))
}

/// Write `value` as JSON to `path`, readable only by its owner, through a
/// temp file and a rename.
///
/// `~/.claude.json` carries the user's own Claude state and the reference
/// writes it `0600`; a readiness run is not the thing that widens that. The
/// rename keeps a reader from seeing half a document, and the temp file
/// carries the real extension so a watcher does not try to parse it.
///
/// The document was read before this call and the rename replaces whatever is
/// there now, so a *running* Claude Code that writes its own state in between
/// loses that write. `expected` is the text it was read as, and the file is
/// re-read here, immediately before the rename, so a write that landed in
/// between refuses this one instead of being deleted by it — the check
/// [`unchanged_since`] describes, the same one the config writers run. What
/// survives is the window of two syscalls between that recheck and the
/// rename, and the honest instruction for the rest is the one `--approve`
/// already implies: it edits a file another process owns, so close that
/// process first. [`approve`] is only reached under the explicit flag, for
/// the one workspace named on the command line, which is what keeps that
/// window from mattering by accident.
fn write_private(path: &Path, expected: &str, value: &Value) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // Same reasoning as `write_atomically`: a dotfiles-managed config is a
    // symlink, and a rename onto the link would clear no gate — the agent
    // reads the file the link points at.
    let path = &resolve_target(path);
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("cannot serialise {}: {e}", path.display()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // `0600` at creation rather than a chmod after the write: the chmod
    // leaves the temp readable at whatever the umask allowed for as long as
    // the write takes, and this file is Claude's own state. The mode is set
    // before a byte of it exists on disk.
    let tmp = temp_for(path);
    let mut temp = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    temp.write_all(format!("{text}\n").as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(temp);
    // `""` is the sentinel the caller's read uses for a file that is not
    // there: a trust write is what creates `~/.claude.json` on a machine
    // where Claude Code has never run.
    if !unchanged_since(path, expected, "") {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "{} changed while this run was preparing it — not touched; close any running Claude Code and re-run",
            path.display()
        ));
    }
    fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} into {}: {e}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn config(value: Value) -> Value {
        value
    }

    /// A directory this test owns. Named per test, so two of them cannot
    /// share one, and emptied on the way in so a rerun starts clean.
    fn own_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pixel-approve-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    #[test]
    fn a_codex_approval_for_an_unresolvable_workspace_refuses_before_asking_the_server() {
        // The order matters and is the point of the assertion: a path that
        // cannot be resolved must not reach the app-server at all, or a typo
        // in `--workspace` becomes an RPC against a folder nobody named.
        let approval = approve(
            Path::new("/nonexistent-home"),
            Agent::Codex,
            Path::new("/nonexistent-workspace-does-not-exist"),
            Duration::from_secs(1),
        );
        assert!(!approval.approved, "{approval:?}");
        assert_eq!(approval.agent, "codex");
        assert!(
            approval
                .detail
                .contains("/nonexistent-workspace-does-not-exist"),
            "the refusal must name the path it could not resolve: {approval:?}"
        );
    }

    #[test]
    fn a_claude_approval_records_both_facts_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = own_dir("claude-fresh");
        let workspace = own_dir("claude-fresh-workspace");
        // The command records the workspace's canonical path, so the key to
        // read back is that one and not the path handed in.
        let real = fs::canonicalize(&workspace).unwrap();
        let key = real.to_string_lossy();
        let approval = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(approval.approved, "{approval:?}");

        let path = home.join(CLAUDE_ONBOARDING_FILE);
        let mode = fs::metadata(&path)
            .expect("the file was written")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "~/.claude.json must stay owner-only");
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["hasCompletedOnboarding"], Value::Bool(true));
        assert_eq!(
            written["projects"][key.as_ref()]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );

        // A second run has nothing left to do and says so, rather than
        // reporting a write it did not make.
        let again = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(again.approved, "{again:?}");
        assert!(
            again.detail.contains("already"),
            "an idempotent run must read as one: {again:?}"
        );
        fs::remove_dir_all(&home).unwrap();
        fs::remove_dir_all(&workspace).unwrap();
    }

    /// The same link on the file `--approve` writes. `~/.claude.json` is a
    /// common dotfiles target, so a rename onto the link would leave the file
    /// Claude Code actually reads still gated while the report said the
    /// workspace was trusted.
    #[test]
    fn a_claude_approval_through_a_symlinked_config_still_clears_the_gate() {
        let home = own_dir("claude-symlinked");
        let workspace = own_dir("claude-symlinked-workspace");
        let real = home.join("managed-claude.json");
        fs::write(&real, "{}").unwrap();
        let path = home.join(CLAUDE_ONBOARDING_FILE);
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let approval = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(approval.approved, "{approval:?}");

        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a regular file"
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(written["hasCompletedOnboarding"], Value::Bool(true));
        fs::remove_dir_all(&home).unwrap();
        fs::remove_dir_all(&workspace).unwrap();
    }

    /// The entry is keyed on the workspace's real path. Written under the
    /// string the caller typed, a relative or symlinked `--workspace`
    /// recorded an entry Claude Code never looks up: the report said trusted
    /// and the prompt still stood.
    #[test]
    fn a_claude_approval_records_the_workspace_by_its_canonical_path() {
        let home = own_dir("claude-canonical");
        let workspace = own_dir("claude-canonical-workspace");
        let link = std::env::temp_dir().join(format!(
            "pixel-approve-{}-claude-canonical-link",
            std::process::id()
        ));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&workspace, &link).expect("create the symlink");

        let approval = approve(&home, Agent::Claude, &link, Duration::from_secs(1));
        assert!(approval.approved, "{approval:?}");

        let written: Value =
            serde_json::from_str(&fs::read_to_string(home.join(CLAUDE_ONBOARDING_FILE)).unwrap())
                .unwrap();
        let real = fs::canonicalize(&workspace).unwrap();
        assert_eq!(
            written["projects"][real.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            Value::Bool(true),
            "the entry is keyed on the real path: {written}"
        );
        assert!(
            written["projects"][link.to_string_lossy().as_ref()].is_null(),
            "the symlink is not a key Claude Code reads: {written}"
        );
        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(&workspace).unwrap();
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_claude_approval_refuses_a_file_it_cannot_parse_instead_of_replacing_it() {
        let home = own_dir("claude-corrupt");
        // A real directory: with an unresolvable one the refusal would be
        // about the path and this test would pass without ever reaching the
        // parse it is named for.
        let workspace = own_dir("claude-corrupt-workspace");
        let path = home.join(CLAUDE_ONBOARDING_FILE);
        fs::write(&path, "not json at all").unwrap();
        let approval = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(!approval.approved, "{approval:?}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "not json at all",
            "a file this command cannot read is not a file it may overwrite"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_unreadable_config_is_refused_rather_than_taken_for_an_absent_one() {
        // Only `NotFound` means "no config yet, create one". Every other read
        // failure is a file that exists and cannot be read, and treating it
        // as absent is the destructive case: the write path would then put a
        // two-key document where the user's whole config used to be.
        let home = own_dir("claude-unreadable");
        let workspace = own_dir("claude-unreadable-workspace");
        let path = home.join(CLAUDE_ONBOARDING_FILE);
        fs::create_dir(&path).unwrap();
        let approval = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(!approval.approved, "{approval:?}");
        assert!(
            approval.detail.contains("cannot read"),
            "the refusal names the read that failed: {approval:?}"
        );
        assert!(
            path.is_dir(),
            "the path it could not read is left exactly as it was"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    /// A trust write is built from a document read earlier, and the rename
    /// replaces whatever is there at that moment. A *running* Claude Code
    /// writing its own state is the other writer this file really has, so the
    /// write refuses instead of deleting what it wrote.
    #[test]
    fn a_claude_trust_write_is_refused_when_the_file_moved_under_it() {
        let home = own_dir("claude-moved");
        let path = home.join(CLAUDE_ONBOARDING_FILE);
        fs::write(&path, r#"{"hasCompletedOnboarding":false}"#).unwrap();
        let expected = fs::read_to_string(&path).unwrap();
        // The agent commits its own state while this run prepares its output.
        let theirs = r#"{"hasCompletedOnboarding":false,"numStartups":7}"#;
        fs::write(&path, theirs).unwrap();

        let err = write_private(
            &path,
            &expected,
            &serde_json::json!({ "hasCompletedOnboarding": true }),
        )
        .expect_err("a stale trust write must not be committed");

        assert!(
            err.contains("changed while this run was preparing it"),
            "the refusal names the conflict: {err}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            theirs,
            "the running agent's own state was overwritten"
        );
        let left: Vec<_> = fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsStr::new(CLAUDE_ONBOARDING_FILE)],
            "the refused write left its temp file behind"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_claude_approval_refuses_a_projects_table_of_the_wrong_shape() {
        let home = own_dir("claude-shape");
        let workspace = own_dir("claude-shape-workspace");
        let path = home.join(CLAUDE_ONBOARDING_FILE);
        fs::write(&path, r#"{"projects": "not a table"}"#).unwrap();
        let approval = approve(&home, Agent::Claude, &workspace, Duration::from_secs(1));
        assert!(!approval.approved, "{approval:?}");
        assert!(
            fs::read_to_string(&path).unwrap().contains("not a table"),
            "the file was replaced: {approval:?}"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_config_that_already_records_both_facts_needs_no_write() {
        let existing = config(serde_json::json!({
            "hasCompletedOnboarding": true,
            "projects": { "/work/repo": { "hasTrustDialogAccepted": true, "other": 7 } },
        }));
        assert_eq!(
            claude_trusted(&existing, "/work/repo").unwrap(),
            None,
            "an already-trusted workspace must not be rewritten"
        );
    }

    #[test]
    fn a_missing_onboarding_flag_is_added_and_the_rest_is_kept() {
        let existing = config(serde_json::json!({
            "projects": { "/work/repo": { "hasTrustDialogAccepted": true } },
            "theme": "dark",
        }));
        let next = claude_trusted(&existing, "/work/repo")
            .unwrap()
            .expect("a write is owed");
        assert_eq!(next["hasCompletedOnboarding"], Value::Bool(true));
        assert_eq!(
            next["theme"],
            Value::String("dark".to_string()),
            "an unrelated key must survive the write"
        );
        assert_eq!(
            next["projects"]["/work/repo"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn a_missing_trust_flag_is_added_alongside_a_workspace_the_file_never_saw() {
        let existing = config(serde_json::json!({
            "hasCompletedOnboarding": true,
            "projects": { "/work/other": { "hasTrustDialogAccepted": true } },
        }));
        let next = claude_trusted(&existing, "/work/repo")
            .unwrap()
            .expect("a write is owed");
        assert_eq!(
            next["projects"]["/work/repo"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
        assert_eq!(
            next["projects"]["/work/other"]["hasTrustDialogAccepted"],
            Value::Bool(true),
            "trusting one workspace must not forget another"
        );
    }

    #[test]
    fn an_empty_config_gains_both_facts_for_the_named_workspace() {
        let next = claude_trusted(&Value::Object(Map::new()), "/work/repo")
            .unwrap()
            .expect("a write is owed");
        assert_eq!(next["hasCompletedOnboarding"], Value::Bool(true));
        assert_eq!(
            next["projects"]["/work/repo"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn a_trust_flag_that_is_not_true_is_not_trust() {
        // `false` is the interesting one: it is what Claude writes for a
        // workspace the user explicitly declined, and reading it as trust
        // would leave the prompt standing while the report claimed otherwise.
        let existing = config(serde_json::json!({
            "hasCompletedOnboarding": true,
            "projects": { "/work/repo": { "hasTrustDialogAccepted": false } },
        }));
        let next = claude_trusted(&existing, "/work/repo")
            .unwrap()
            .expect("a write is owed");
        assert_eq!(
            next["projects"]["/work/repo"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn a_config_that_is_not_an_object_is_refused_rather_than_replaced() {
        assert!(claude_trusted(&Value::String("nonsense".to_string()), "/work/repo").is_err());
        assert!(claude_trusted(&serde_json::json!([1, 2]), "/work/repo").is_err());
    }

    #[test]
    fn a_projects_value_of_the_wrong_shape_is_refused_rather_than_overwritten() {
        let existing = config(serde_json::json!({ "projects": "not a table" }));
        let err = claude_trusted(&existing, "/work/repo").unwrap_err();
        assert!(err.contains("projects"), "{err}");
    }

    #[test]
    fn an_agent_with_no_approval_path_says_so_instead_of_claiming_success() {
        for agent in [Agent::Antigravity, Agent::Devin] {
            let approval = approve(
                Path::new("/nonexistent-home"),
                agent,
                Path::new("/work/repo"),
                Duration::from_secs(1),
            );
            assert!(!approval.approved, "{approval:?}");
            assert_eq!(approval.agent, agent.name());
            assert!(
                approval.detail.contains("no approval path"),
                "the absence of a write must read as a decision: {approval:?}"
            );
        }
    }

    #[test]
    fn an_already_recorded_trust_level_is_read_back() {
        assert!(workspace_trusted_in(
            "[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n",
            "/work/repo"
        ));
    }

    #[test]
    fn a_config_without_the_projects_table_is_not_trusted() {
        assert!(!workspace_trusted_in("model = \"x\"\n", "/work/repo"));
    }

    #[test]
    fn an_explicit_refusal_is_not_trust() {
        assert!(!workspace_trusted_in(
            "[projects.\"/work/repo\"]\ntrust_level = \"untrusted\"\n",
            "/work/repo"
        ));
    }

    #[test]
    fn another_folders_trust_does_not_cover_this_one() {
        assert!(!workspace_trusted_in(
            "[projects.\"/work/other\"]\ntrust_level = \"trusted\"\n",
            "/work/repo"
        ));
    }

    #[test]
    fn a_trust_level_of_the_wrong_type_is_not_trust() {
        assert!(!workspace_trusted_in(
            "[projects.\"/work/repo\"]\ntrust_level = true\n",
            "/work/repo"
        ));
    }

    #[test]
    fn an_unparsable_config_is_not_evidence_of_trust() {
        assert!(!workspace_trusted_in("this is not toml [", "/work/repo"));
    }

    /// A `codex` app-server that answers `hooks/list` from a fixture and
    /// records every `config/batchWrite` made through it. The real one is a
    /// spawned child speaking a line protocol; this stands in for it so the
    /// policy above it is reachable at all.
    struct FakeAppServer {
        hooks: Value,
        writes: Mutex<Vec<(String, Value, MergeStrategy)>>,
    }

    impl FakeAppServer {
        fn answering(hooks: Value) -> Self {
            Self {
                hooks,
                writes: Mutex::new(Vec::new()),
            }
        }

        fn keys(&self) -> Vec<String> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .map(|(key, _, _)| key.clone())
                .collect()
        }
    }

    impl CodexRpc for FakeAppServer {
        fn hooks(&self) -> Result<Vec<rpc::HookEntry>, String> {
            Ok(rpc::parse_hook_entries(&self.hooks))
        }

        fn write(
            &self,
            key_path: String,
            value: Value,
            merge_strategy: MergeStrategy,
        ) -> Result<(), String> {
            self.writes
                .lock()
                .unwrap()
                .push((key_path, value, merge_strategy));
            Ok(())
        }
    }

    /// A Codex `hooks/list` answer carrying one enabled hook the server
    /// reported a current hash for, which is what makes hook state non-empty.
    fn one_trustable_hook() -> Value {
        serde_json::json!([{ "hooks": [{
            "enabled": true,
            "trust_status": "untrusted",
            "key": "session-start",
            "current_hash": "abc123",
        }] }])
    }

    #[test]
    fn an_empty_hook_state_is_not_written_but_the_trust_level_still_is() {
        // The two writes are independent, and the report has to keep them so.
        // Writing an empty `hooks.state` would be a write that records
        // nothing and a line claiming hooks were trusted when none were.
        let home = own_dir("codex-empty-hooks");
        let config = home.join("config.toml");
        let server = FakeAppServer::answering(serde_json::json!([{ "hooks": [] }]));

        let detail =
            clear_codex_with("/work/repo", &config, &server).expect("the exchange succeeds");

        assert_eq!(
            server.keys(),
            vec![rpc::trust_key_path("/work/repo")],
            "only the trust level is written: {detail}"
        );
        assert!(
            !detail.contains("hook(s) trusted"),
            "no hook was trusted, so nothing may say one was: {detail}"
        );
        assert!(detail.contains("/work/repo marked trusted"), "{detail}");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn hook_state_is_written_once_per_hook_the_server_reported_a_hash_for() {
        let home = own_dir("codex-hooks");
        let config = home.join("config.toml");
        let server = FakeAppServer::answering(one_trustable_hook());

        let detail =
            clear_codex_with("/work/repo", &config, &server).expect("the exchange succeeds");

        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert_eq!(writes[0].0, HOOKS_STATE_KEY);
        assert_eq!(writes[0].2, MergeStrategy::Upsert);
        assert_eq!(
            writes[0].1["session-start"]["trusted_hash"],
            Value::String("abc123".to_string()),
            "the hash written is the one the server computed, not one read from disk"
        );
        assert_eq!(writes[1].0, rpc::trust_key_path("/work/repo"));
        assert_eq!(writes[1].2, MergeStrategy::Replace);
        assert!(detail.contains("1 enabled hook(s) trusted"), "{detail}");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_workspace_the_config_already_records_as_trusted_is_not_written_again() {
        let home = own_dir("codex-already-trusted");
        let config = home.join("config.toml");
        fs::write(
            &config,
            "[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        let server = FakeAppServer::answering(serde_json::json!([{ "hooks": [] }]));

        let detail =
            clear_codex_with("/work/repo", &config, &server).expect("the exchange succeeds");

        assert!(
            server.keys().is_empty(),
            "a trust the config already records is not re-granted: {detail}"
        );
        assert!(detail.contains("was already trusted"), "{detail}");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_workspace_the_config_does_not_record_as_trusted_is_marked_trusted() {
        let home = own_dir("codex-not-trusted");
        let config = home.join("config.toml");
        fs::write(
            &config,
            "[projects.\"/work/other\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        let server = FakeAppServer::answering(serde_json::json!([{ "hooks": [] }]));

        let detail =
            clear_codex_with("/work/repo", &config, &server).expect("the exchange succeeds");

        assert_eq!(
            server.keys(),
            vec![rpc::trust_key_path("/work/repo")],
            "another folder's trust does not cover this one: {detail}"
        );
        assert!(detail.contains("/work/repo marked trusted"), "{detail}");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_hook_the_server_cannot_hash_refuses_the_whole_write() {
        let home = own_dir("codex-unhashable");
        let config = home.join("config.toml");
        let server = FakeAppServer::answering(serde_json::json!([{ "hooks": [{
            "enabled": true,
            "trust_status": "untrusted",
            "key": "session-start",
        }] }]));

        let err = clear_codex_with("/work/repo", &config, &server).unwrap_err();

        assert!(
            server.keys().is_empty(),
            "a partial map is not written: {err}"
        );
        assert!(err.contains("no current hash"), "{err}");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn two_writers_of_one_file_never_share_a_temp_path() {
        // The bug this pins: the temp name was fixed, so two tests writing
        // the same `~/.claude.json` in parallel shared `<home>/.claude.json
        // .tmp` and the first rename took it away from the second — a run
        // that reported failing to rename a file it had just written.
        let path = Path::new("/home/someone/.claude.json");
        let first = temp_for(path);
        let second = temp_for(path);
        assert_ne!(first, second, "two writers must not share one temp path");
        assert_eq!(
            first.parent(),
            path.parent(),
            "the rename is within the one directory"
        );
        assert_eq!(
            first.extension(),
            Some(std::ffi::OsStr::new("tmp")),
            "the temp file is not spelled like the file it becomes: {}",
            first.display()
        );
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".claude.json"),
            "{}",
            first.display()
        );
    }
}
