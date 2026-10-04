// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The per-agent config writers.
//!
//! Each writer merges rather than replaces: an agent's config holds the
//! user's own settings (Codex's `developer_instructions`, the Claude hooks
//! `pixel install` writes, Antigravity's workspace trust list), and a
//! readiness run must not be the thing that deletes them. Every write is a
//! read-modify-write through a parser, to a temp file, then a rename — an
//! agent may be reading the file while we rewrite it.

use std::{
    fs,
    io::{self, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::Path,
};

// Codex's own config file name, owned by `pixel_install::codex_config`
// because `pixel install` reads and writes that same file: importing it is
// the only spelling, so a rename there cannot leave this writer behind.
use pixel_install::codex_config::CODEX_CONFIG_FILE;
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use super::agents::DEFAULT_CLAUDE_MODEL;
use super::provider::Provider;

/// Codex's provider name for the recording route. One literal name, so a
/// Codex reinstall never has to guess which name the apply path wrote.
pub(crate) const MODEL_PROVIDER_NAME: &str = "recording_cloud";

/// Where the recording gateway listens. Claude and Antigravity are pointed
/// at it; Codex goes straight at the provider's own OpenAI-compatible base.
pub(crate) const GATEWAY_URL: &str = "http://127.0.0.1:4000";

/// The env var the gateway reads its own bearer token from — the name
/// `gateway.yaml` spells (`master_key: os.environ/RECORDING_GATEWAY_KEY`),
/// so the report tells the user to export a variable their gateway actually
/// reads. A name that resolves to nothing exports an empty token, which
/// reaches the proxy unauthenticated and looks exactly like a wrong key.
///
/// It belongs to the **shell**, not to a config file: Claude Code reads a
/// settings-file `env` value directly from the file without expanding it,
/// and a settings-file value then replaces the same variable inherited from
/// the shell — so writing `${RECORDING_GATEWAY_KEY}` there injects that
/// literal string and clobbers the real credential. The apply path leaves
/// both this and the header to the shell and names them in its report
/// instead.
pub(crate) const GATEWAY_TOKEN_ENV: &str = "RECORDING_GATEWAY_KEY";

/// The env var Claude Code reads extra request headers from. Claude
/// authenticates against the Anthropic API with its own `x-api-key`, and a
/// run pointed at the gateway with the base URL alone presents neither that
/// nor the proxy's key, so it arrives unauthenticated.
///
/// The header is what the reference exports, and it works: the route accepts
/// `x-litellm-api-key: Bearer …`, `x-api-key: …` and `Authorization: Bearer
/// …` alike (all answered 200 on 2026-09-30; no credential at all answered
/// 500). It is not the *only* one that works — `ANTHROPIC_AUTH_TOKEN` alone
/// authenticates — which is why the report names both.
pub(crate) const CLAUDE_CUSTOM_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";

/// The value [`CLAUDE_CUSTOM_HEADERS_ENV`] must carry. It resolves nothing
/// in a config file, so this is the exact line the report tells the user to
/// export — with the token left as a shell expansion, which is where that
/// expansion actually happens.
pub(crate) fn gateway_header() -> String {
    format!("x-litellm-api-key: Bearer ${{{GATEWAY_TOKEN_ENV}}}")
}

/// The model slots Claude Code reads. All four are pinned to the alias the
/// probe uses, and `CLAUDE_SUBAGENT_MODEL_ENV` with them: the gateway's model
/// list carries that alias, not Anthropic's own model names, so a slot left at
/// the user's default asks the proxy for a model it cannot map and comes back
/// 404. An unpinned subagent is the same 404 one turn later.
pub(crate) const CLAUDE_MODEL_SLOTS: [&str; 4] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
];

/// The env var Claude Code picks a subagent's model from, outside the four
/// slots above because it is not a slot a user sets but a separate override.
pub(crate) const CLAUDE_SUBAGENT_MODEL_ENV: &str = "CLAUDE_CODE_SUBAGENT_MODEL";

pub(crate) const CLAUDE_SETTINGS_FILE: &str = ".claude/settings.json";
pub(crate) const DEVIN_CONFIG_FILE: &str = ".config/devin/config.json";

/// Claude's own state file — a dotfile beside `~/.claude/`, not
/// `settings.json` inside it. This is where Claude records that onboarding
/// finished and that a folder's trust dialog was accepted, which is what the
/// approval path has to answer; `CLAUDE_SETTINGS_FILE` above is the file this
/// command writes the provider route into. Two files, two jobs.
pub(crate) const CLAUDE_ONBOARDING_FILE: &str = ".claude.json";

/// The Antigravity CLI's own settings, distinct from the IDE's
/// `~/.gemini/config/config.json` that `pixel install` writes plugins into.
pub(crate) fn antigravity_settings(home: &Path) -> std::path::PathBuf {
    home.join(".gemini/antigravity-cli/settings.json")
}

pub(crate) fn claude_settings(home: &Path) -> std::path::PathBuf {
    home.join(CLAUDE_SETTINGS_FILE)
}

pub(crate) fn codex_config(home: &Path) -> std::path::PathBuf {
    home.join(".codex").join(CODEX_CONFIG_FILE)
}

pub(crate) fn devin_config(home: &Path) -> std::path::PathBuf {
    home.join(DEVIN_CONFIG_FILE)
}

/// Read a file that may not exist yet. A missing file is an empty starting
/// point, not an error: the first readiness run is what creates it.
fn read_or(path: &Path, missing: &str) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(missing.to_string()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// True while `path` still holds `expected` — the text [`read_or`] returned
/// to the caller that built its merged document from it.
///
/// Every writer here reads a document, merges its own keys into it and writes
/// the whole thing back, so a write built from a stale read commits the old
/// document and takes the other writer's change with it: a hook Claude Code
/// added while this run was preparing its output would be gone, with both
/// writes reporting success. This is the check both writers run between the
/// read and the rename. `absent` is the sentinel the caller gave [`read_or`],
/// so a file that is still missing compares equal to what the caller read and
/// a first run can create it, while a file that appeared or lost content
/// since does not compare equal. A destination that cannot be read at all is
/// not unchanged: the write is refused either way, and "changed" is the
/// outcome the caller can act on.
pub(crate) fn unchanged_since(path: &Path, expected: &str, absent: &str) -> bool {
    read_or(path, absent).is_ok_and(|current| current == expected)
}

/// Write `text` through a temp file and a rename, creating the directory,
/// but only while `path` still holds `expected` — see [`unchanged_since`] for
/// what that protects and why `absent` is needed beside it.
///
/// The temp is created with the destination's own mode, or `0600` when there
/// is no destination yet. `fs::write` would take the mode from the umask, and
/// the rename then hands that mode to the destination: an agent settings file
/// the user had narrowed to `0600` would come back `0644`, and the keys and
/// hooks in it would be readable by every account on the machine. A mode is
/// not this command's to widen, and a file it creates may hold the user's own
/// credentials, so the narrower default is the one it chooses.
///
/// The temp's name is unique per writer ([`temp_for`]), so two runs against
/// the same config cannot take the file out from under each other — which
/// rules out a collision, not a lost update. The recheck is what covers the
/// second, and it runs after the temp is complete and immediately before the
/// rename, so the window is two syscalls wide rather than a whole scan's
/// worth. Closing it entirely would need a lock, which only serializes the
/// writers that take it, and the realistic one here — the agent rewriting its
/// own settings — does not.
fn write_atomically(path: &Path, expected: &str, absent: &str, text: String) -> Result<(), String> {
    let path = &resolve_target(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = temp_for(path);
    // `create_new`: the temp name is unique to this writer, so a file that
    // is already there is a leftover from a crashed run rather than
    // something to write through. Refusing it is loud; writing through it
    // would leave the old document's tail on the end of the new one.
    let mut temp = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode_for(path))
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    temp.write_all(text.as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(temp);
    if !unchanged_since(path, expected, absent) {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "{} changed while this run was preparing it — not touched; re-run to merge into the current file",
            path.display()
        ));
    }
    fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} into {}: {e}", tmp.display(), path.display()))
}

/// `path` with a final symlink resolved to the file it points at.
///
/// A dotfiles manager makes an agent's config a symlink into the user's own
/// repository, and the rename in [`write_atomically`] replaces whatever sits
/// at the destination. Renaming onto the link would delete the link, leave
/// the managed file at its old contents and still report a write: the agent
/// reads the file the link pointed at, which never changed. Resolving first
/// puts the rename on that file — which is also the one `read_or` read and
/// [`mode_for`] measured, both of which already follow the link.
///
/// Only a link is resolved, so an ordinary path is written where the caller
/// named it and the report names the same path it was given. A link whose
/// target is gone resolves to nothing and is returned unchanged: the write
/// then leaves a regular file where the link was, as it did before.
pub(crate) fn resolve_target(path: &Path) -> std::path::PathBuf {
    fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.file_type().is_symlink())
        .and_then(|_| fs::canonicalize(path).ok())
        .unwrap_or_else(|| path.to_path_buf())
}

/// The mode a rewrite of `path` carries: the one it already has, so a rename
/// cannot widen it, and `0600` for a file this command is the first to create.
fn mode_for(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path).map_or(0o600, |meta| meta.permissions().mode())
}

/// Distinguishes two writers of the same path inside one process.
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The temp path a write to `path` goes through.
///
/// Unique per writer rather than fixed: two callers writing the same file
/// would otherwise share `<path>.tmp`, and the first `rename` takes it out
/// from under the second, which then reports a rename that failed on a file
/// it wrote itself. Two tests in one binary are enough to hit that, and two
/// concurrent processes are the same race with a longer window. The name
/// keeps the real one inside it (`.claude.json.<pid>.<n>.tmp`) so a watcher
/// looking for `*.json` does not try to parse a half-written document.
pub(crate) fn temp_for(path: &Path) -> std::path::PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{seq}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Point Codex at `provider`: the model, the provider name, and the
/// `[model_providers.recording_cloud]` block that carries the base URL and
/// the name of the env var holding the key.
pub(crate) fn write_codex(path: &Path, provider: Provider) -> Result<String, String> {
    let original = read_or(path, "")?;
    // The parser error is dropped rather than interpolated: `toml_edit`'s
    // `TomlError` prints the offending line next to its caret, and this file
    // is the user's own — a `[mcp_servers.*]` block holds tokens, and an
    // unterminated string on such a line would put one in a diagnostic that
    // the report prints and a user pastes into an issue. The path and the
    // verdict are what the caller acts on.
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|_| format!("{} is not valid TOML — not touched", path.display()))?;
    doc["model"] = Item::Value(TomlValue::from(provider.model()));
    doc["model_provider"] = Item::Value(TomlValue::from(MODEL_PROVIDER_NAME));
    let mut route = Table::new();
    route["name"] = Item::Value(TomlValue::from(MODEL_PROVIDER_NAME));
    route["base_url"] = Item::Value(TomlValue::from(provider.base_url()));
    route["env_key"] = Item::Value(TomlValue::from(provider.key_env()));
    // `responses` is the only value current Codex accepts: its `WireApi` enum
    // has that one variant, and `"chat"` is a config-load error
    // (`CHAT_WIRE_API_REMOVED_ERROR`, openai/codex discussion 7782), so a
    // `"chat"` here would leave Codex refusing to start rather than talking a
    // different protocol. Ollama serves it — `POST /v1/responses`, added in
    // v0.13.3, stateless only, which is all the recordings ask of it.
    route["wire_api"] = Item::Value(TomlValue::from("responses"));
    // Merged into, never replaced. `[model_providers.<other>]` blocks are the
    // user's own routes, and leaving them alone is what the sibling
    // `write_claude` does with `env`; assigning a fresh table over this key
    // deleted every one of them on an `--apply`. The same applies to this
    // route's own name: a user who tuned `recording_cloud` by hand owns the
    // fields this command does not write — Codex reads `request_max_retries`
    // and `stream_idle_timeout_ms` there — and the four below are the ones
    // `--apply` replaces.
    let root = doc.as_table_mut();
    if !root.contains_key("model_providers") {
        root.insert("model_providers", Item::Table(Table::new()));
    }
    let providers = root
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            format!(
                "`model_providers` in {} is not a table — not touched",
                path.display()
            )
        })?;
    if providers.contains_key(MODEL_PROVIDER_NAME) {
        // The four fields above are this command's; every other key in the
        // table is the user's, and a second `--apply` has to leave them
        // alone just as the first one did. A name holding something that is
        // not a table is refused for the same reason `model_providers` is.
        let existing = providers
            .get_mut(MODEL_PROVIDER_NAME)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| {
                format!(
                    "`model_providers.{MODEL_PROVIDER_NAME}` in {} is not a table — not touched",
                    path.display()
                )
            })?;
        for (key, value) in route.iter() {
            existing.insert(key, value.clone());
        }
    } else {
        providers.insert(MODEL_PROVIDER_NAME, Item::Table(route));
    }
    write_atomically(path, &original, "", doc.to_string())?;
    Ok(format!(
        "model={} via {} in {}",
        provider.model(),
        provider.name(),
        path.display()
    ))
}

/// Point Claude Code at the recording gateway. The `env` object is merged
/// into: whatever the user or `pixel install` put there survives.
pub(crate) fn write_claude(path: &Path, provider: Provider) -> Result<String, String> {
    let original = read_or(path, "{}")?;
    let mut value: Value = serde_json::from_str(&original)
        .map_err(|e| format!("{} is not valid JSON: {e} — not touched", path.display()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object — not touched", path.display()))?;
    let env = match object
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(map) => map,
        // Covers both a user's `"env": "…"` and any non-object written
        // there: either way the file is left exactly as it was found.
        _ => {
            return Err(format!(
                "`env` in {} is not an object — not touched",
                path.display()
            ));
        }
    };
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        Value::String(GATEWAY_URL.to_string()),
    );
    // The credential and the header are deliberately *not* written here.
    // Claude Code reads a settings-file `env` value literally — it expands
    // nothing — and that literal then replaces the same variable inherited
    // from the shell, so an entry like `${RECORDING_GATEWAY_TOKEN}` would
    // both fail to resolve and destroy the working value. The two names go
    // out in the report instead, for the user to export. Keys a user already
    // has are left alone, as everywhere else here.
    for slot in CLAUDE_MODEL_SLOTS {
        env.insert(
            slot.to_string(),
            Value::String(DEFAULT_CLAUDE_MODEL.to_string()),
        );
    }
    env.insert(
        CLAUDE_SUBAGENT_MODEL_ENV.to_string(),
        Value::String(DEFAULT_CLAUDE_MODEL.to_string()),
    );
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    write_atomically(path, &original, "{}", format!("{text}\n"))?;
    Ok(format!(
        "ANTHROPIC_BASE_URL={GATEWAY_URL}, {DEFAULT_CLAUDE_MODEL} in the model slots, via {} in {}; export ANTHROPIC_AUTH_TOKEN=\"${GATEWAY_TOKEN_ENV}\" and {CLAUDE_CUSTOM_HEADERS_ENV}=\"{}\" in your shell — a settings file expands neither",
        provider.name(),
        path.display(),
        gateway_header()
    ))
}

/// Point the Antigravity CLI at the recording gateway, creating its settings
/// file when the CLI has never run.
pub(crate) fn write_antigravity(path: &Path, provider: Provider) -> Result<String, String> {
    let original = read_or(path, "{}")?;
    let mut value: Value = serde_json::from_str(&original)
        .map_err(|e| format!("{} is not valid JSON: {e} — not touched", path.display()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object — not touched", path.display()))?;
    object.insert(
        "AGY_LLM_GATEWAY_URL".to_string(),
        Value::String(GATEWAY_URL.to_string()),
    );
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    write_atomically(path, &original, "{}", format!("{text}\n"))?;
    Ok(format!(
        "AGY_LLM_GATEWAY_URL={GATEWAY_URL} via {} in {}",
        provider.name(),
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory removed on drop. `tempfile` is not a dependency of
    /// this crate and one test-local RAII guard is cheaper than adding it.
    pub(crate) struct Scratch(std::path::PathBuf);

    impl Scratch {
        pub(crate) fn new() -> Self {
            // The counter, not the clock, is what makes the name unique.
            // `SystemTime::now()` resolves to a microsecond or coarser
            // depending on the platform, so two tests starting in the same
            // tick were handed the same directory: each wrote its fixtures
            // into the other's and the pair failed intermittently, on a
            // different test each run.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("pixel-readify-{}-{seq}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn codex_gets_the_model_provider_and_route_block() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        write_codex(&path, Provider::Ollama).unwrap();
        let doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("deepseek-v4.1-flash"));
        assert_eq!(doc["model_provider"].as_str(), Some(MODEL_PROVIDER_NAME));
        let route = &doc["model_providers"][MODEL_PROVIDER_NAME];
        assert_eq!(route["base_url"].as_str(), Some("https://ollama.com/v1"));
        assert_eq!(route["env_key"].as_str(), Some("OLLAMA_API_KEY"));
        assert_eq!(route["wire_api"].as_str(), Some("responses"));
    }

    /// A dotfiles manager makes the config a symlink (`chezmoi`, `stow`), and
    /// the agent reads the file on the other end. A rename onto the link
    /// replaces the link with a regular file and leaves that other file at
    /// its old contents: the run reports a write the agent never sees. The
    /// assertion on the link is what pins it — reading `path` back would find
    /// the new contents either way.
    #[test]
    fn a_symlinked_config_is_rewritten_at_its_target() {
        let home = Scratch::new();
        let real = home.path().join("managed-settings.json");
        fs::write(&real, "{}").unwrap();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        write_claude(&path, Provider::Ollama).unwrap();

        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a regular file"
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(written["env"]["ANTHROPIC_BASE_URL"], GATEWAY_URL);
    }

    #[test]
    fn codex_keeps_the_users_own_keys() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "developer_instructions = \"pixel prompt\"\nmodel = \"stale\"\napproval_policy = \"never\"\n",
        )
        .unwrap();
        write_codex(&path, Provider::Ollama).unwrap();
        let doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["developer_instructions"].as_str(), Some("pixel prompt"));
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(doc["model"].as_str(), Some("deepseek-v4.1-flash"));
    }

    /// A user's own provider blocks are theirs. This writer owns exactly one
    /// route and rewrites that; the first version assigned a fresh table over
    /// the whole `model_providers` key, so an `--apply` deleted every other
    /// provider the user had. `codex_keeps_the_users_own_keys` did not catch
    /// it: it writes only top-level keys, so the bug survived a test named
    /// for the thing it broke.
    #[test]
    fn codex_keeps_a_users_other_model_providers() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[model_providers.local_llama]\nname = \"llama\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
        )
        .unwrap();
        write_codex(&path, Provider::Ollama).unwrap();
        let doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["model_providers"]["local_llama"]["base_url"].as_str(),
            Some("http://127.0.0.1:11434/v1"),
            "the provider the user already had survived: {doc}"
        );
        assert_eq!(
            doc["model_providers"][MODEL_PROVIDER_NAME]["base_url"].as_str(),
            Some("https://ollama.com/v1")
        );
    }

    /// The route's own table belongs to the user as much as its siblings do:
    /// the four fields above are the only ones this command owns. Assigning a
    /// fresh table over the name deleted the rest of it — Codex itself reads
    /// `request_max_retries` and `stream_idle_timeout_ms` from `[model_providers]` —
    /// so a user who tuned this route lost that tuning to an `--apply` that
    /// reported a write.
    #[test]
    fn codex_keeps_the_fields_it_does_not_own_in_its_own_route() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "[model_providers.{MODEL_PROVIDER_NAME}]\n\
                 name = \"hand tuned\"\n\
                 base_url = \"http://stale.invalid/v1\"\n\
                 request_max_retries = 7\n\
                 stream_idle_timeout_ms = 90000\n"
            ),
        )
        .unwrap();
        write_codex(&path, Provider::Ollama).unwrap();
        let doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        let route = &doc["model_providers"][MODEL_PROVIDER_NAME];
        assert_eq!(
            route["request_max_retries"].as_integer(),
            Some(7),
            "a setting this command does not own survived the rewrite: {doc}"
        );
        assert_eq!(
            route["stream_idle_timeout_ms"].as_integer(),
            Some(90000),
            "{doc}"
        );
        // The four it does own are rewritten rather than left as found, so
        // this is a merge and not a `skip when present`.
        assert_eq!(route["name"].as_str(), Some(MODEL_PROVIDER_NAME), "{doc}");
        assert_eq!(
            route["base_url"].as_str(),
            Some("https://ollama.com/v1"),
            "{doc}"
        );
        assert_eq!(route["env_key"].as_str(), Some("OLLAMA_API_KEY"), "{doc}");
        assert_eq!(route["wire_api"].as_str(), Some("responses"), "{doc}");
    }

    /// A scalar under the route's own name is refused rather than clobbered,
    /// for the reason the sibling above is: this command writes one route and
    /// has nothing to merge into a value that is not a table.
    #[test]
    fn codex_refuses_a_route_name_that_is_not_a_table() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = format!("[model_providers]\n{MODEL_PROVIDER_NAME} = \"nonsense\"\n");
        fs::write(&path, &broken).unwrap();
        let error = write_codex(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("is not a table"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "file touched");
    }

    /// `model_providers` holding something that is not a table is refused
    /// rather than clobbered: this command writes one route, and a value it
    /// cannot merge into is a file it must leave alone.
    #[test]
    fn codex_refuses_a_model_providers_that_is_not_a_table() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "model_providers = \"nonsense\"\n";
        fs::write(&path, broken).unwrap();
        let error = write_codex(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("not a table"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "file touched");
    }

    #[test]
    fn codex_refuses_to_rewrite_a_file_that_does_not_parse() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "this is not = = toml\n";
        fs::write(&path, broken).unwrap();
        let error = write_codex(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("not valid TOML"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "file touched");
    }

    /// The parser error is not interpolated into the message. `toml_edit`
    /// prints the offending line beside its caret, and this file holds the
    /// user's own `[mcp_servers.*]` blocks and whatever tokens they carry, so
    /// an unterminated string on such a line would put a credential into a
    /// diagnostic the report prints and a user pastes into an issue.
    #[test]
    fn a_malformed_line_never_carries_its_text_into_the_error() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "[mcp_servers.notes]\nbearer_token = \"sk-live-not-a-real-key\n";
        fs::write(&path, broken).unwrap();
        let error = write_codex(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("is not valid TOML"), "{error}");
        assert!(
            !error.contains("sk-live-not-a-real-key"),
            "the line's own text reached the diagnostic: {error}"
        );
    }

    #[test]
    fn codex_refuses_a_config_it_cannot_read_rather_than_calling_it_missing() {
        // A missing file is an empty starting point — that is what lets the
        // first readiness run create one. Only a *missing* file is: a path
        // that exists and cannot be read has to surface as the read error,
        // because treating it as empty would write a fresh config over
        // whatever is really there. A directory where the file belongs is
        // the read failure a test can arrange on every platform.
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(&path).unwrap();
        let error = write_codex(&path, Provider::Ollama)
            .expect_err("a path that cannot be read is not a missing config");
        assert!(
            error.starts_with(&format!("read {}", path.display())),
            "the read failure is the one to report: {error}"
        );
        assert!(path.is_dir(), "the refused path is left as it was");
    }

    #[test]
    fn codex_apply_is_idempotent() {
        let home = Scratch::new();
        let path = codex_config(home.path());
        write_codex(&path, Provider::Ollama).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        write_codex(&path, Provider::Ollama).unwrap();
        assert_eq!(first, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn claude_gets_the_gateway_and_the_model_slots() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        write_claude(&path, Provider::Ollama).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], GATEWAY_URL);
        for slot in CLAUDE_MODEL_SLOTS {
            assert_eq!(
                value["env"][slot], DEFAULT_CLAUDE_MODEL,
                "{slot} left at the user's default asks the gateway for a model it cannot map"
            );
        }
        assert_eq!(
            value["env"][CLAUDE_SUBAGENT_MODEL_ENV], DEFAULT_CLAUDE_MODEL,
            "an unpinned subagent is the same 404 one turn later"
        );
    }

    #[test]
    fn claude_leaves_the_credential_to_the_shell() {
        // The load-bearing half of the fix. Claude Code expands nothing in a
        // settings-file `env` value, and such a value replaces the same
        // variable inherited from the shell — so an entry here would inject
        // the literal `${RECORDING_GATEWAY_TOKEN}` *and* overwrite the real
        // credential the shell already exported. Neither key may appear.
        let home = Scratch::new();
        let path = claude_settings(home.path());
        write_claude(&path, Provider::Ollama).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for key in [CLAUDE_CUSTOM_HEADERS_ENV, "ANTHROPIC_AUTH_TOKEN"] {
            assert!(
                value["env"].get(key).is_none(),
                "{key} must stay in the shell: {}",
                value["env"]
            );
        }
    }

    #[test]
    fn the_report_names_the_two_exports_the_shell_owes() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        let report = write_claude(&path, Provider::Ollama).unwrap();
        assert!(report.contains(GATEWAY_TOKEN_ENV), "{report}");
        assert!(report.contains(CLAUDE_CUSTOM_HEADERS_ENV), "{report}");
        assert!(report.contains(&gateway_header()), "{report}");
    }

    #[test]
    fn the_gateway_header_names_the_litellm_key_and_expands_the_token() {
        // The header name is the contract with the proxy: LiteLLM reads
        // `x-litellm-api-key`, and a spelling drift here is a 400 from the
        // gateway, not a silent fallback. Pinned literally for that reason,
        // and the token is pinned as an expansion — a shell expands it, which
        // is the whole reason this line is an export and not a file entry.
        // The variable name is spelled out rather than built from
        // `GATEWAY_TOKEN_ENV`: an expectation that interpolates the constant
        // it checks passes whatever that constant becomes, which is the
        // drift this test exists to catch.
        assert_eq!(
            gateway_header(),
            "x-litellm-api-key: Bearer ${RECORDING_GATEWAY_KEY}"
        );
    }

    #[test]
    fn the_settings_file_never_names_the_token() {
        // Stronger than "an expansion, not a value": the token's name does
        // not belong in this file at all. An expansion here resolves to
        // nothing *and* replaces the shell's real value, so the file that
        // must not carry the secret is the file that must not mention it.
        let home = Scratch::new();
        let path = claude_settings(home.path());
        write_claude(&path, Provider::Ollama).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(GATEWAY_TOKEN_ENV), "{text}");
        assert!(!text.contains("litellm"), "{text}");
    }

    #[test]
    fn claude_keeps_the_keys_pixel_install_wrote() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"env":{"PIXEL_MARKER":"1","ANTHROPIC_BASE_URL":"http://old"},"hooks":{"Stop":[]}}"#,
        )
        .unwrap();
        write_claude(&path, Provider::Ollama).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["env"]["PIXEL_MARKER"], "1");
        assert!(value["hooks"]["Stop"].is_array());
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], GATEWAY_URL);
    }

    #[test]
    fn claude_refuses_a_settings_file_that_does_not_parse() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let error = write_claude(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[test]
    fn claude_refuses_an_env_that_is_not_an_object() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"env":"nope"}"#).unwrap();
        let error = write_claude(&path, Provider::Ollama).unwrap_err();
        assert!(error.contains("not an object"), "{error}");
    }

    #[test]
    fn antigravity_gets_the_gateway_and_keeps_workspace_trust() {
        let home = Scratch::new();
        let path = antigravity_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"trustedWorkspaces":["/w"]}"#).unwrap();
        write_antigravity(&path, Provider::Ollama).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["AGY_LLM_GATEWAY_URL"], GATEWAY_URL);
        assert_eq!(value["trustedWorkspaces"][0], "/w");
    }

    #[test]
    fn antigravity_creates_the_file_when_the_cli_never_ran() {
        let home = Scratch::new();
        let path = antigravity_settings(home.path());
        assert!(!path.exists());
        write_antigravity(&path, Provider::Ollama).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["AGY_LLM_GATEWAY_URL"], GATEWAY_URL);
    }

    #[test]
    fn the_antigravity_ide_config_is_a_different_file_from_the_cli_settings() {
        let home = Scratch::new();
        assert_ne!(
            antigravity_settings(home.path()),
            home.path().join(".gemini/config/config.json"),
            "the CLI reads its own settings, not the IDE's"
        );
    }

    #[test]
    fn devin_config_path_is_the_verified_only_target() {
        let home = Scratch::new();
        assert_eq!(
            devin_config(home.path()),
            home.path().join(DEVIN_CONFIG_FILE)
        );
    }

    /// The mode of `path`, permission bits only.
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_rewrite_does_not_widen_a_file_the_user_narrowed() {
        use std::os::unix::fs::PermissionsExt as _;

        // The settings file holds whatever the user put in it, and a rename
        // carries the temp's mode onto the destination. `fs::write` takes
        // that mode from the umask, so a `0600` file would come back `0644`
        // and every other account on the machine could read it.
        let home = Scratch::new();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"env\":{\"KEEP\":\"mine\"}}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_claude(&path, Provider::Ollama).unwrap();

        assert_eq!(mode_of(&path), 0o600, "the rewrite widened the file");
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value["env"]["KEEP"], "mine",
            "preserving the mode must not have cost the contents"
        );
    }

    #[test]
    fn a_file_this_command_creates_is_readable_by_its_owner_alone() {
        // Nothing recorded a mode for a file that did not exist. `0644` is
        // the umask default and this is a config an agent reads back, so the
        // narrower choice is the one that cannot leak.
        let home = Scratch::new();
        let path = codex_config(home.path());
        write_codex(&path, Provider::Ollama).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn a_shorter_rewrite_replaces_the_whole_document() {
        // The rename replaces the file rather than writing into it, so a
        // document shorter than the one it replaces must not leave the
        // previous one's tail behind. `model` is the fixture because the
        // writer overwrites it with a short literal: `developer_
        // instructions` survives the merge by design and would prove
        // nothing about the tail.
        let home = Scratch::new();
        let path = codex_config(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("model = \"{}\"\n", "x".repeat(4096))).unwrap();
        write_codex(&path, Provider::Ollama).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("xxx"),
            "a tail of the previous document survived the rewrite: {text}"
        );
        let doc: DocumentMut = text.parse().expect("the file still parses");
        assert_eq!(doc["model"].as_str(), Some("deepseek-v4.1-flash"));
    }

    /// A write built from a read that another writer has since invalidated is
    /// refused, not committed.
    ///
    /// The transaction `write_claude` runs is `read_or` and then
    /// `write_atomically`, and this test drives those two in that order with
    /// the other writer landing in between. It has to be built by hand: the
    /// pair is two calls inside one function, so there is no seam to inject a
    /// real concurrent write through without a watcher and a race that
    /// reports success on a fast machine and failure on a loaded one. What it
    /// asserts is what the caller would see: the error says the file moved,
    /// and the other writer's bytes are still the ones on disk.
    #[test]
    fn a_change_between_the_read_and_the_write_is_refused_not_overwritten() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"env\":{\"KEEP\":\"1\"}}").unwrap();

        // The read half, exactly as `write_claude` opens.
        let original = read_or(&path, "{}").unwrap();
        // The other writer — Claude Code adding a hook — commits first.
        let theirs = "{\"hooks\":{\"Stop\":[{\"command\":\"pixel run-hook guard\"}]}}";
        fs::write(&path, theirs).unwrap();

        let err = write_atomically(&path, &original, "{}", "{}\n".to_string())
            .expect_err("a stale write must not be committed");
        assert!(
            err.contains("changed while this run was preparing it"),
            "the error must name the conflict, not something the user would chase: {err}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            theirs,
            "the other writer's update was overwritten"
        );
        let left: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            vec![path.file_name().unwrap().to_os_string()],
            "the refused write left its temp file behind"
        );
    }

    /// The same recheck must not block the first run, when the file the
    /// writer read as missing is still missing by the time it renames.
    #[test]
    fn a_first_run_write_is_not_a_conflict() {
        let home = Scratch::new();
        let path = claude_settings(home.path());
        assert!(!path.exists());
        write_claude(&path, Provider::Ollama).unwrap();
        assert!(path.exists(), "the sentinel read was mistaken for a change");
    }
}
