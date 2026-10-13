// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel config` — persistent layered settings.
//!
//! The `metrics` key controls whether the live 🟩 footer is emitted on
//! stderr after an ordinary command. Settings are opt-out layers: the
//! nearest scope wins — a repo-level `on` overrides a global `off` — and
//! an unset layer defaults to on. `--metrics=off` and `PIXEL_METRICS=0`
//! still veto a single invocation above every file layer.
//!
//! The `policy` key is the retired guard's switch. No hook reads it any more:
//! the guard and its rewrite or deny of native retrieval are gone, and
//! `pixel install` registers only the task-event hook. The key stays readable
//! and writable so an existing configuration file keeps validating, and
//! `pixel config` still reports it. `PIXEL_POLICY` overrides it for one
//! environment and the legacy `PIXEL_TARGETS_GUARD=0` kill switch still reads
//! as `off`, both for that report only.
//!
//! YAML files live under `.pixel/` in the repository and home directory.
//! Legacy JSON remains readable until install/edit creates the YAML equivalent.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::prompt_key::{KeyReader, RawGuard};
use serde_json::{Value, json};

/// Settings key of the daemon auto-start opt-out (`daemon_auto_start: false`).
pub(crate) const DAEMON_AUTO_START_FEATURE: &str = "daemon_auto_start";
/// Its environment opt-out: `0`, `false` or `off`.
pub(crate) const DAEMON_AUTO_START_ENV: &str = "PIXEL_DAEMON_AUTO_START";

/// The environment overrides and YAML switches exposed in the overview.
const FEATURES: &[(&str, &str)] = &[
    (DAEMON_AUTO_START_FEATURE, DAEMON_AUTO_START_ENV),
    ("task_context", "PIXEL_TASK_CONTEXT"),
    ("task_boundary", "PIXEL_TASK_BOUNDARY"),
    (
        crate::execution_brief::chain::BRIEF_FEATURE,
        crate::execution_brief::chain::BRIEF_ENV,
    ),
];

/// Configuration layers nearest first: the repository file, then the global one.
fn layers(root: Option<&Path>) -> impl Iterator<Item = PathBuf> {
    root.map(repo_config_path)
        .into_iter()
        .chain(global_config_path())
}

fn resolved(root: Option<&Path>, key: &str) -> (Option<bool>, String) {
    for path in layers(root) {
        if let Some(value) = read_config_doc(&path).and_then(|doc| doc.get(key)?.as_bool()) {
            return (Some(value), path.display().to_string());
        }
    }
    (None, "default".into())
}

/// Environment overrides win; absent feature switches preserve the enabled baseline.
pub fn feature_enabled(root: Option<&Path>, key: &str, env: &str) -> bool {
    feature_resolution(root, key, env).0
}

fn feature_resolution(root: Option<&Path>, key: &str, env: &str) -> (bool, String) {
    if let Ok(value) = std::env::var(env) {
        return (!matches!(value.as_str(), "0" | "false" | "off"), env.into());
    }
    let (value, source) = resolved(root, key);
    (value.unwrap_or(true), source)
}

/// The retired guard's setting, kept so existing configuration still parses.
/// Nothing acts on it. The three values are the YAML setting, the `--json`
/// value, and the `pixel config policy` argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum PolicyMode {
    /// The default.
    Advisory,
    /// Accepted for configurations written by older releases; no hook reads it.
    Enforce,
    /// The legacy off switch; no hook reads it.
    Off,
}

impl PolicyMode {
    /// The exact setting spelling, as stored in YAML and printed in JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Enforce => "enforce",
            Self::Off => "off",
        }
    }

    /// Parse one configuration value; an unknown spelling is not a policy.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "advisory" => Some(Self::Advisory),
            "enforce" => Some(Self::Enforce),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The layer that decided the effective policy.
enum PolicySource {
    /// The environment variable that overrode every file layer.
    Environment(&'static str),
    Repo,
    Global,
    Default,
}

/// Effective retrieval policy and the layer that set it.
struct PolicyResolution {
    mode: PolicyMode,
    source: PolicySource,
    /// The file that declared the setting, for the two file layers.
    file: Option<PathBuf>,
}

impl PolicyResolution {
    fn environment(mode: PolicyMode, variable: &'static str) -> Self {
        Self {
            mode,
            source: PolicySource::Environment(variable),
            file: None,
        }
    }

    /// Layer name for `--json`: a fixed word the Pi extension can branch on.
    fn source_name(&self) -> &'static str {
        match self.source {
            PolicySource::Environment(_) => "env",
            PolicySource::Repo => "repo",
            PolicySource::Global => "global",
            PolicySource::Default => "default",
        }
    }

    /// `pixel config policy` reports the layer in words.
    fn layer(&self) -> String {
        match (&self.source, &self.file) {
            (PolicySource::Environment(variable), _) => format!("{variable} (environment)"),
            (_, Some(file)) => format!("{} {}", self.source_name(), file.display()),
            _ => "default (no config sets it)".into(),
        }
    }

    /// The overview line names the variable or file the value came from.
    fn overview_label(&self) -> String {
        match (&self.source, &self.file) {
            (PolicySource::Environment(variable), _) => (*variable).into(),
            (_, Some(file)) => file.display().to_string(),
            _ => "default".into(),
        }
    }
}

/// The policy one layer declares, or `None` when the layer does not
/// pronounce itself (missing file, missing key, or unknown value). An
/// unknown value is ignored rather than fatal: a hook must never break a
/// session over a typo, and `pixel config` reports it when the file is
/// validated.
fn read_policy(path: &Path) -> Option<PolicyMode> {
    PolicyMode::parse(read_config_doc(path)?.get("policy")?.as_str()?)
}

/// Effective retrieval policy for `root`: the `PIXEL_POLICY` environment
/// override, the repository layer, the global layer, then the advisory
/// default. `PIXEL_TARGETS_GUARD=0` (also `false`/`off`) remains the
/// legacy kill switch over everything.
fn policy_resolution(root: Option<&Path>) -> PolicyResolution {
    const LEGACY_SWITCH: &str = "PIXEL_TARGETS_GUARD";
    if crate::env_flag_off(LEGACY_SWITCH) {
        return PolicyResolution::environment(PolicyMode::Off, LEGACY_SWITCH);
    }
    if let Ok(value) = std::env::var("PIXEL_POLICY") {
        // An unrecognised value selects the advisory default, as it always has.
        let mode = PolicyMode::parse(value.trim().to_ascii_lowercase().as_str())
            .unwrap_or(PolicyMode::Advisory);
        return PolicyResolution::environment(mode, "PIXEL_POLICY");
    }
    let repo = root.map(repo_config_path);
    if let Some(mode) = repo.as_deref().and_then(read_policy) {
        return PolicyResolution {
            mode,
            source: PolicySource::Repo,
            file: repo,
        };
    }
    let global = global_config_path();
    if let Some(mode) = global.as_deref().and_then(read_policy) {
        return PolicyResolution {
            mode,
            source: PolicySource::Global,
            file: global,
        };
    }
    PolicyResolution {
        mode: PolicyMode::Advisory,
        source: PolicySource::Default,
        file: None,
    }
}

pub fn ensure_template(root: Option<&Path>) -> Result<PathBuf, String> {
    let directory = if let Some(root) = root {
        root.join(".pixel")
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for the global config")?)
            .join(".pixel")
    };
    let path = directory.join(crate::config_file::FILE_NAME);
    crate::config_file::ensure(&path)?;
    Ok(path)
}

pub fn edit(path: &Path, repo: bool) -> Result<(), String> {
    let root = if repo {
        Some(crate::discover_root(path)?)
    } else {
        None
    };
    let path = ensure_template(root.as_deref())?;
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".into());
    let args = shell_words::split(&editor).map_err(|_| "invalid quoting in VISUAL/EDITOR")?;
    let (program, args) = args.split_first().ok_or("empty editor command")?;
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status()
        .map_err(|e| format!("launch editor: {e}"))?;
    if !status.success() {
        return Err(format!("editor exited with {status}"));
    }
    validate(&path)?;
    println!("configuration: {}", path.display());
    Ok(())
}

fn validate(path: &Path) -> Result<(), String> {
    let doc = crate::config_file::load(path)?;
    for (key, _) in FEATURES {
        if doc.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(format!("{}: {key} must be true or false", path.display()));
        }
    }
    if doc
        .get("metrics")
        .is_some_and(|v| !matches!(v.as_str(), Some("on" | "off")))
    {
        return Err(format!("{}: metrics must be on or off", path.display()));
    }
    if doc
        .get("policy")
        .is_some_and(|v| !v.as_str().and_then(PolicyMode::parse).is_some())
    {
        return Err(format!(
            "{}: policy must be advisory, enforce, or off",
            path.display()
        ));
    }
    classify_enabled_in(&doc)?;
    if let Some(classify) = doc.get("classify") {
        if !classify.is_object() {
            return Err(format!("{}: classify must be a mapping", path.display()));
        }
        if classify
            .get("engine")
            .is_some_and(|v| !matches!(v.as_str(), Some("auto" | "local" | "remote")))
        {
            return Err(format!(
                "{}: classify.engine must be auto, local, or remote",
                path.display()
            ));
        }
        if classify.get("remote_preset").is_some_and(|v| {
            v.as_str()
                .and_then(crate::decide_remote::Preset::parse_name)
                .is_none()
        }) {
            return Err(format!(
                "{}: unknown classify.remote_preset",
                path.display()
            ));
        }
    }
    if let Some(web_search) = doc.get("web_search") {
        if !web_search.is_object() {
            return Err(format!("{}: web_search must be a mapping", path.display()));
        }
        if web_search
            .get("searxng_url")
            .is_some_and(|v| !v.as_str().is_some_and(|s| !s.is_empty()))
        {
            return Err(format!(
                "{}: web_search.searxng_url must be a non-empty string",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Print only known public settings; arbitrary configuration can contain secrets.
pub fn overview(path: &Path) -> Result<(), String> {
    let root = crate::discover_root(path).ok();
    let global = global_config_path().ok_or("no HOME for the global config")?;
    println!("global: {}", global.display());
    validate(&global)?;
    if let Some(root) = root.as_deref() {
        let path = repo_config_path(root);
        println!("repo: {}", path.display());
        validate(&path)?;
    }
    let (metrics, source) = metrics_resolution(root.as_deref());
    if std::env::var_os("PIXEL_METRICS").is_some_and(|v| v == "0") {
        println!("metrics: off (PIXEL_METRICS)");
    } else {
        println!(
            "metrics: {} ({source:?})",
            if metrics { "on" } else { "off" }
        );
    }
    for (key, env) in FEATURES {
        let (enabled, source) = feature_resolution(root.as_deref(), key, env);
        println!("{key}: {enabled} ({source})");
    }
    let policy = policy_resolution(root.as_deref());
    println!(
        "policy: {} ({})",
        policy.mode.as_str(),
        policy.overview_label()
    );
    println!("classify.enabled: {} (global)", classify_enabled()?);
    println!(
        "classify.engine: {}",
        classify_engine().unwrap_or_else(|| "auto (default)".into())
    );
    if let Some(preset) = classify_remote_preset() {
        println!("classify.remote_preset: {}", preset.display());
    }
    println!(
        "web-search provider: {}",
        crate::web_search::configured_provider()
    );
    let doc = crate::config_file::load(&global)?;
    if let Some(keys) = doc.get("remote_keys").and_then(Value::as_object) {
        for (name, value) in keys {
            // Provider names are user input too: print only recognized presets.
            if let Some(preset) = crate::decide_remote::Preset::parse_name(name) {
                let name = preset.display();
                println!(
                    "remote_keys.{name}: {}",
                    if value.as_str().is_some_and(|s| !s.is_empty()) {
                        "set"
                    } else {
                        "unset"
                    }
                );
            }
        }
    }
    println!("Setup: pixel config setup (interactive global settings)");
    println!("Edit: pixel config edit (global), pixel config edit --repo (repository)");
    Ok(())
}

/// The metrics setting one layer declares, or `None` when the layer does
/// not pronounce itself (missing file, missing key, or malformed value).
fn read_metrics(path: &Path) -> Option<bool> {
    let value = read_config_doc(path)?;
    match value.get("metrics")?.as_str()? {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn repo_config_path(root: &Path) -> PathBuf {
    crate::config_file::preferred_path(&root.join(".pixel"))
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| crate::config_file::preferred_path(&PathBuf::from(home).join(".pixel")))
}

/// Effective live-metrics setting for `root`: repo layer first, then the
/// global configuration, then the on-by-default baseline.
pub fn metrics_enabled(root: Option<&Path>) -> bool {
    if let Some(root) = root
        && let Some(on) = read_metrics(&repo_config_path(root))
    {
        return on;
    }
    global_config_path()
        .as_deref()
        .and_then(read_metrics)
        .unwrap_or(true)
}

/// Which layer produced the effective setting — for `pixel config metrics`.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Repo,
    Global,
    Default,
}

fn metrics_resolution(root: Option<&Path>) -> (bool, Source) {
    if let Some(root) = root
        && let Some(on) = read_metrics(&repo_config_path(root))
    {
        return (on, Source::Repo);
    }
    if let Some(path) = global_config_path()
        && let Some(on) = read_metrics(&path)
    {
        return (on, Source::Global);
    }
    (true, Source::Default)
}

fn write_doc(path: &Path, mutate: impl FnOnce(&mut Value)) -> Result<(), String> {
    let before = crate::config_file::load(path)?;
    let mut doc = before.clone();
    mutate(&mut doc);
    let rendered = crate::config_file::render(path, &before, &doc)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // A tmp name shared across concurrent invocations could rename one
    // command's content as another's — scope it to this process. Same-process
    // callers serialize on ENV_LOCK in tests; the pid separates real ones.
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.json"),
        std::process::id(),
    ));
    if let Err(e) = write_private(&tmp, rendered.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", path.display()));
    }
    Ok(())
}

/// Write `bytes` to a new file only its owner can read. The global config
/// holds provider API keys and the rename keeps the new inode's mode, so
/// every write — not only the one storing a key — must create it 0600.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // A leftover tmp from a crashed run would keep its old mode.
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// The key a `pixel config remote-key` value stands for: `-` reads the
/// first line of `stdin`, which keeps the secret out of shell history and
/// `ps`; anything else is the key itself.
pub fn key_from_arg(
    value: Option<String>,
    stdin: &mut dyn BufRead,
) -> Result<Option<String>, String> {
    if value.as_deref() != Some("-") {
        return Ok(value);
    }
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("remote-key: read stdin: {e}"))?;
    Ok(Some(line.trim().to_string()))
}

fn write_metrics(path: &Path, on: bool) -> Result<(), String> {
    write_doc(path, |doc| {
        doc["metrics"] = Value::String(if on { "on" } else { "off" }.to_string());
    })
}

/// Global kill switch: checked before classify reads input or opens an engine.
pub fn classify_enabled() -> Result<bool, String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    classify_enabled_in(&crate::config_file::load(&path)?)
}

fn classify_enabled_in(doc: &Value) -> Result<bool, String> {
    match doc.get("classify").and_then(|c| c.get("enabled")) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "classify.enabled must be true or false".into()),
    }
}

/// Save a global classify switch while retaining engine and credential settings.
pub fn set_classify_enabled(enabled: bool) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    set_classify_enabled_at(&path, enabled)
}

fn set_classify_enabled_at(path: &Path, enabled: bool) -> Result<(), String> {
    write_doc(path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["enabled"] = json!(enabled);
    })
}

/// Terminal adapter shared by explicit setup and interactive global installation.
#[cfg_attr(test, mutants::skip)] // Terminal and provider adapters; draft/save policy tested with injected I/O.
pub fn setup() -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(
            "setup needs a terminal; use pixel config edit or pixel config classify off".into(),
        );
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let path = ensure_template(None)?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stderr().lock();
    let keys = KeyReader::enable();
    let saved = setup_with_install(
        &path,
        &mut input,
        &mut output,
        color,
        &keys,
        |input, output| crate::classify_setup::install_step(true, input, output),
        |input, output| crate::web_search_setup::install_step(true, input, output),
    )?;
    let root = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::discover_root(&cwd).ok());
    note_repo_override(&mut output, saved, read_metrics(&path), root.as_deref())
}

/// Keep a failed first-time engine installation from enabling classification.
/// Returns whether the settings were saved. The web-search step runs
/// whenever the settings save, independent of the classify answer — the two
/// configure unrelated features; the classify installer is gated on the
/// classify answer and a failure reverts only a first-time opt-in.
fn setup_with_install(
    path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    color: bool,
    keys: &KeyReader,
    install: impl FnOnce(&mut dyn BufRead, &mut dyn Write) -> Result<(), String>,
    install_web_search: impl FnOnce(&mut dyn BufRead, &mut dyn Write) -> Result<(), String>,
) -> Result<bool, String> {
    let was_enabled = classify_enabled_in(&crate::config_file::load(path)?)?;
    if !setup_with_keys(path, input, output, color, keys)? {
        return Ok(false);
    }
    // The web-search and classify steps configure unrelated features, so a
    // web-search failure must not keep the classify installer from running.
    // Run the web-search step, hold any error, and report it only after the
    // classify step has had its turn.
    let web_search_result = install_web_search(input, output);
    if !classify_enabled_in(&crate::config_file::load(path)?)? {
        web_search_result?;
        return Ok(true);
    }
    if let Err(error) = install(input, output) {
        if !was_enabled {
            set_classify_enabled_at(path, false).map_err(|rollback| {
                format!("{error}; could not disable classification: {rollback}")
            })?;
        }
        return Err(error);
    }
    web_search_result?;
    Ok(true)
}

/// Setup and `config metrics on --global` write the global layer only, and a
/// repo-level `metrics` still wins in its repository; without this note a
/// stale legacy repo `config.json` defeats the answer the user just gave and
/// the footer stays hidden. Best effort: no repository, no saved answer, or a
/// repo layer that agrees means no note.
fn note_repo_override(
    output: &mut dyn Write,
    saved: bool,
    saved_on: Option<bool>,
    root: Option<&Path>,
) -> Result<(), String> {
    let Some(saved_on) = saved_on.filter(|_| saved) else {
        return Ok(());
    };
    let Some(root) = root else {
        return Ok(());
    };
    let path = repo_config_path(root);
    let Some(repo_on) = read_metrics(&path) else {
        return Ok(());
    };
    if repo_on == saved_on {
        return Ok(());
    }
    writeln!(
        output,
        "note: {} sets metrics: {} and wins in this repository — run `pixel config metrics {}` here to apply this answer",
        path.display(),
        if repo_on { "on" } else { "off" },
        if saved_on { "on" } else { "off" },
    )
    .map_err(|e| e.to_string())
}

/// The classify question's shown default: a previously saved choice wins
/// (an explicit opt-out must not be re-asked as yes); an unset value
/// defaults to yes, so the feature is offered, not hidden.
fn classify_default_in(doc: &Value) -> Result<bool, String> {
    match doc.get("classify").and_then(|c| c.get("enabled")) {
        None => Ok(true),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "classify.enabled must be true or false".into()),
    }
}

/// The short classify explanation shown before the setup question.
fn classify_blurb(color: bool) -> String {
    format!(
        "{} is optional AI classification, separate from code search.\n\
         Ask a bounded question about text and get probabilities for your labels:\n\n{}\n{}\n\n\
         Local runs offline after setup. Remote providers receive your text and may charge.",
        paint(color, "1", "Classify"),
        paint(
            color,
            "2",
            "  pixel classify \"Which of these two methods handles null safely?\" \\"
        ),
        paint(
            color,
            "2",
            "      --label 'early-return' --label 'Optional'"
        )
    )
}

/// Line-driven setup: the path the tests take, mirroring what a
/// non-terminal run would render.
#[cfg(test)]
fn setup_with(
    path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    color: bool,
) -> Result<bool, String> {
    setup_with_keys(path, input, output, color, &KeyReader::inert())
}

fn setup_with_keys(
    path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    color: bool,
    keys: &KeyReader,
) -> Result<bool, String> {
    validate(path)?;
    let mut doc = crate::config_file::load(path)?;
    writeln!(output).map_err(|e| e.to_string())?;
    writeln!(
        output,
        "{}",
        paint(color, "1;32", "Pixel setup — global settings")
    )
    .map_err(|e| e.to_string())?;
    writeln!(output, "File: {}", path.display()).map_err(|e| e.to_string())?;
    let hint = if keys.active() {
        "←/→ pick · Enter confirms · y or n answers outright · q or Ctrl-C cancels without saving\n\
         Repository and environment overrides still apply."
    } else {
        "Enter keeps the shown value · q or Ctrl-D cancels without saving\n\
         Repository and environment overrides still apply."
    };
    writeln!(output, "{}", paint(color, "2", hint)).map_err(|e| e.to_string())?;
    let metrics = doc.get("metrics").and_then(Value::as_str) != Some("off");
    let Some(metrics) = ask_bool(
        input,
        output,
        "Show command timing and estimated savings?",
        metrics,
        keys,
        color,
    )?
    else {
        return Ok(false);
    };
    doc["metrics"] = json!(if metrics { "on" } else { "off" });
    for (key, label) in [
        (
            "daemon_auto_start",
            "Start the background repository daemon on demand?",
        ),
        (
            "task_context",
            "Suggest relevant code when an agent receives a prompt (retired: no hook reads this)?",
        ),
        (
            "task_boundary",
            "Detect task changes in agent prompts (retired: no hook reads this)?",
        ),
    ] {
        let current = doc.get(key).and_then(Value::as_bool).unwrap_or(true);
        let Some(value) = ask_bool(input, output, label, current, keys, color)? else {
            return Ok(false);
        };
        doc[key] = json!(value);
    }
    let Some(enforce) = ask_bool(
        input,
        output,
        "Enforce Pixel retrieval policy (retired: no hook reads this)?",
        doc.get("policy").and_then(Value::as_str) == Some(PolicyMode::Enforce.as_str()),
        keys,
        color,
    )?
    else {
        return Ok(false);
    };
    doc["policy"] = json!(if enforce { "enforce" } else { "advisory" });
    writeln!(output).map_err(|e| e.to_string())?;
    writeln!(output, "{}", classify_blurb(color)).map_err(|e| e.to_string())?;
    let Some(enabled) = ask_bool(
        input,
        output,
        "Allow pixel classify?",
        classify_default_in(&doc)?,
        keys,
        color,
    )?
    else {
        return Ok(false);
    };
    let review = [
        (
            "metrics",
            doc["metrics"].as_str().unwrap_or("?").to_string(),
        ),
        (
            "daemon auto-start",
            yes_no(doc["daemon_auto_start"].as_bool().unwrap_or(true)),
        ),
        (
            "task context",
            yes_no(doc["task_context"].as_bool().unwrap_or(true)),
        ),
        (
            "task boundary",
            yes_no(doc["task_boundary"].as_bool().unwrap_or(true)),
        ),
        ("policy", doc["policy"].as_str().unwrap_or("?").to_string()),
        (
            "classify",
            if enabled {
                "enabled".to_string()
            } else {
                "disabled".to_string()
            },
        ),
    ];
    let key_width = review
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    writeln!(output).map_err(|e| e.to_string())?;
    for (key, value) in review {
        writeln!(
            output,
            "  {}  {}",
            paint(color, "2", &format!("{key:<key_width$}")),
            paint(color, "1", &value)
        )
        .map_err(|e| e.to_string())?;
    }
    // Enter must save: the header promises "Enter keeps the shown value",
    // and a save question defaulting to no silently discarded every answer
    // the user had just given.
    if ask_bool(input, output, "Save these settings?", true, keys, color)? != Some(true) {
        return Ok(false);
    }
    write_doc(path, |current| {
        for key in [
            "metrics",
            "daemon_auto_start",
            "task_context",
            "task_boundary",
            "policy",
        ] {
            current[key] = doc[key].clone();
        }
        if !current.get("classify").is_some_and(Value::is_object) {
            current["classify"] = json!({});
        }
        current["classify"]["enabled"] = json!(enabled);
    })?;
    writeln!(
        output,
        "Saved. Run pixel config to see effective settings or pixel config setup to change them."
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Wrap `text` in the SGR `code` when `color` is on; pass it through plain
/// otherwise. NO_COLOR is resolved by the caller.
fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// `yes`/`no` for the review block, so the booleans read as words.
fn yes_no(value: bool) -> String {
    if value {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

/// One yes/no question: the arrow picker on a live terminal, line input
/// otherwise. `None` means cancelled.
#[cfg_attr(test, mutants::skip)] // the raw branch needs a real tty; each branch is tested through its own fn
fn ask_bool(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    label: &str,
    current: bool,
    keys: &KeyReader,
    color: bool,
) -> Result<Option<bool>, String> {
    if keys.active()
        && let Some(mut raw) = RawGuard::new()
    {
        return ask_bool_keys(output, label, current, &mut raw, color);
    }
    ask_bool_lines(input, output, label, current)
}

/// The line-driven yes/no question: Enter keeps the shown value, y/n
/// answer, `q` cancels.
fn ask_bool_lines(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    label: &str,
    current: bool,
) -> Result<Option<bool>, String> {
    loop {
        write!(
            output,
            "{label} [{}] > ",
            if current { "Y/n" } else { "y/N" }
        )
        .map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(None);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(Some(current)),
            "y" | "yes" => return Ok(Some(true)),
            "n" | "no" => return Ok(Some(false)),
            "q" => return Ok(None),
            _ => writeln!(output, "Type y, n, Enter, or q.").map_err(|e| e.to_string())?,
        }
    }
}

/// The arrow-driven picker: the highlight starts on the shown value, ←/→
/// move it, Enter confirms, y/n answer outright, q/Esc/Ctrl-C/D cancel.
/// Each keystroke rewrites the question line in place; the active and
/// inactive choice tokens stay the same width so a rewrite never needs to
/// erase the line.
#[cfg_attr(test, mutants::skip)] // raw terminal loop; the key mapping is tested pure in prompt_key
fn ask_bool_keys(
    output: &mut dyn Write,
    label: &str,
    current: bool,
    raw: &mut RawGuard,
    color: bool,
) -> Result<Option<bool>, String> {
    let mut picked = current;
    let hint = paint(color, "2", "  ←/→ pick · Enter confirm · q cancel");
    // A line longer than the terminal wraps; the \r rewrite can then not
    // reach the start of the question and the previous render survives as
    // a duplicate. Drop the hint when the full line would not fit.
    let hint = picker_hint(label, &hint, crate::select::terminal_width());
    render_choice(output, label, picked, &hint, color)?;
    loop {
        match crate::prompt_key::step(raw.read_key(), picked) {
            crate::prompt_key::Step::Highlight(value) => {
                picked = value;
                render_choice(output, label, picked, &hint, color)?;
            }
            crate::prompt_key::Step::Settle(Some(answer)) => {
                picked = answer;
                break;
            }
            crate::prompt_key::Step::Settle(None) => {
                writeln!(output).map_err(|e| e.to_string())?;
                return Ok(None);
            }
        }
    }
    render_choice(output, label, picked, &hint, color)?;
    writeln!(output).map_err(|e| e.to_string())?;
    Ok(Some(picked))
}

/// Decide whether the key hint fits on one line: a rendered line at least as
/// wide as the terminal wraps, which the `\r` rewrite cannot recover from, so
/// the hint is dropped. `xxx / xxx` is a fixed-width stand-in for the choice
/// tokens, whose real width is identical whatever the picked state.
fn picker_hint(label: &str, hint: &str, cols: usize) -> String {
    if crate::select::visible_len(&format!("{label} [ xxx / xxx ]{hint} ")) >= cols {
        String::new()
    } else {
        hint.to_owned()
    }
}

/// One picker line: `[ (Y) / n ]` with the active choice in parentheses and
/// bold — green for yes, yellow for no — the inactive one dim.
fn render_choice(
    output: &mut dyn Write,
    label: &str,
    picked: bool,
    hint: &str,
    color: bool,
) -> Result<(), String> {
    let (yes, no) = choice_tokens(picked, color);
    write!(output, "\r{label} [ {yes} / {no} ]{hint} ").map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())
}

/// The two choice tokens, always the same display width so an in-place
/// rewrite leaves no residue with color on or off.
fn choice_tokens(picked: bool, color: bool) -> (String, String) {
    if picked {
        (paint(color, "1;32", "(Y)"), paint(color, "2", " n "))
    } else {
        (paint(color, "2", " Y "), paint(color, "1;33", "(n)"))
    }
}

/// The stored classify engine preference: `local`, `remote`, or `auto`.
pub fn classify_engine() -> Option<String> {
    global_config_path()
        .as_deref()
        .and_then(read_config_doc)
        .and_then(|doc| {
            doc.get("classify")?
                .get("engine")?
                .as_str()
                .map(str::to_string)
        })
}

/// The provider selected by the interactive remote setup.
pub fn classify_remote_preset() -> Option<crate::decide_remote::Preset> {
    let doc = read_config_doc(&global_config_path()?)?;
    crate::decide_remote::Preset::parse_name(doc.get("classify")?.get("remote_preset")?.as_str()?)
}

/// The model chosen alongside the remote provider, if the setup asked for
/// one (OpenCode Go's subscription carries several).
pub fn classify_remote_model() -> Option<String> {
    let doc = read_config_doc(&global_config_path()?)?;
    doc.get("classify")?
        .get("remote_model")?
        .as_str()
        .map(str::to_string)
}

/// The endpoint base chosen alongside the remote provider, when the setup
/// routed a provider to another host (OpenCode Go's Jev lives on its zen
/// base, not the go chat endpoint).
pub fn classify_remote_base() -> Option<String> {
    let doc = read_config_doc(&global_config_path()?)?;
    doc.get("classify")?
        .get("remote_base")?
        .as_str()
        .map(str::to_string)
}

/// Store the remote engine and its provider together — plus the model and
/// endpoint base the provider should run; `None` drops each earlier choice
/// so a stale one cannot leak across a provider switch.
pub fn set_classify_remote_model(
    preset: crate::decide_remote::Preset,
    model: Option<String>,
    base: Option<&str>,
) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["engine"] = json!("remote");
        doc["classify"]["remote_preset"] = json!(preset.display());
        if let Some(classify) = doc["classify"].as_object_mut() {
            match model.filter(|m| !m.is_empty()) {
                Some(model) => {
                    classify.insert("remote_model".to_string(), json!(model));
                }
                None => {
                    classify.remove("remote_model");
                }
            };
            match base.filter(|b| !b.is_empty()) {
                Some(base) => {
                    classify.insert("remote_base".to_string(), json!(base));
                }
                None => {
                    classify.remove("remote_base");
                }
            };
        }
    })
}

/// The recorded local Ollaya daemon launch (base, model name, env, argv).
pub fn ollaya_launch() -> Option<Value> {
    global_config_path()
        .as_deref()
        .and_then(read_config_doc)
        .and_then(|doc| doc.get("classify")?.get("ollaya").cloned())
}

/// Persist the classify engine preference.
pub fn set_classify_engine(value: &str) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["engine"] = Value::String(value.to_string());
    })
}

/// Persist the local Ollaya server launch record.
pub fn set_ollaya_launch(launch: &Value) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["ollaya"] = launch.clone();
    })
}

fn read_config_doc(path: &Path) -> Option<Value> {
    crate::config_file::load(path).ok()
}

/// The stored API key for a remote decision preset, if the global config
/// carries one. Keys live only in the global configuration under
/// `remote_keys` — never in the repo layer, never echoed back by the CLI.
pub fn remote_key(preset: crate::decide_remote::Preset) -> Option<String> {
    let path = global_config_path()?;
    let doc = read_config_doc(&path)?;
    doc.get("remote_keys")?
        .get(preset.display())?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The SearXNG base URL the web-search setup recorded, if any: the
/// `PIXEL_WEB_SEARCH_URL` env var wins over it in `pixel web-search`.
pub fn web_search_searxng_url() -> Option<String> {
    let path = global_config_path()?;
    read_config_doc(&path)?
        .get("web_search")?
        .get("searxng_url")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Persist the SearXNG base URL chosen by the web-search setup.
pub fn set_web_search_searxng_url(url: &str) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("web_search").is_some_and(Value::is_object) {
            doc["web_search"] = json!({});
        }
        doc["web_search"]["searxng_url"] = json!(url);
    })
}

/// Remove a stored SearXNG URL when the web-search setup switches to
/// another provider. Unlike writing an empty value, this leaves a
/// configuration that `validate` accepts: an empty `searxng_url` is
/// rejected as a non-empty-string violation.
pub fn remove_web_search_searxng_url() -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if let Some(web_search) = doc.get_mut("web_search").and_then(Value::as_object_mut) {
            web_search.remove("searxng_url");
            if web_search.is_empty() {
                doc.as_object_mut().map(|root| root.remove("web_search"));
            }
        }
    })
}

/// The Perplexity API key the web-search setup stored, under
/// `remote_keys.perplexity` — the same secret store, in the same 0600
/// global file, that `pixel config remote-key` writes. Never echoed back.
pub fn web_search_perplexity_key() -> Option<String> {
    let path = global_config_path()?;
    let doc = read_config_doc(&path)?;
    doc.get("remote_keys")?
        .get("perplexity")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Persist the Perplexity API key, mirroring `pixel config remote-key`
/// storage, under `remote_keys.perplexity`.
pub fn set_web_search_perplexity_key(key: &str) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("remote_keys").is_some_and(Value::is_object) {
            doc["remote_keys"] = json!({});
        }
        doc["remote_keys"]["perplexity"] = json!(key);
    })
}

/// `pixel config remote-key <preset> [key]`: with a value, persist it to
/// the global configuration (created 0600 on unix — it holds secrets);
/// without one, report whether a key is stored. `--clear` removes it.
/// The key itself is never printed.
pub fn run_remote_key(
    preset: crate::decide_remote::Preset,
    value: Option<String>,
    clear: bool,
) -> Result<(), String> {
    let name = preset.display();
    let path = global_config_path().ok_or("no HOME for the global config")?;
    if clear {
        write_doc(&path, |doc| {
            if let Some(keys) = doc.get_mut("remote_keys").and_then(Value::as_object_mut) {
                keys.remove(name);
            }
        })?;
        println!("remote-key {name}: cleared — wrote {}", path.display());
        return Ok(());
    }
    match value {
        Some(key) if !key.is_empty() => {
            write_doc(&path, |doc| {
                if !doc.get("remote_keys").is_some_and(Value::is_object) {
                    doc["remote_keys"] = json!({});
                }
                doc["remote_keys"][name] = Value::String(key);
            })?;
            println!("remote-key {name}: set — wrote {}", path.display());
            Ok(())
        }
        Some(_) => Err("remote-key: empty key".to_string()),
        None => {
            let state = if remote_key(preset).is_some() {
                "set"
            } else {
                "unset"
            };
            println!("remote-key {name}: {state} — {}", path.display());
            Ok(())
        }
    }
}

/// `pixel config metrics [on|off] [--global]`: without a value, report the
/// effective setting and the layer that set it; with a value, persist it to
/// the chosen layer.
pub fn run_metrics(path: &Path, global: bool, value: Option<bool>) -> Result<(), String> {
    let mut output = std::io::stdout().lock();
    run_metrics_with(&mut output, path, global, value)
}

fn run_metrics_with(
    output: &mut dyn Write,
    path: &Path,
    global: bool,
    value: Option<bool>,
) -> Result<(), String> {
    let root = crate::discover_root(path).ok();
    match value {
        None => {
            let (on, source) = metrics_resolution(root.as_deref());
            let layer = match source {
                Source::Repo => {
                    format!(
                        "repo {}",
                        repo_config_path(root.as_deref().unwrap()).display()
                    )
                }
                Source::Global => format!(
                    "global {}",
                    global_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                Source::Default => "default (no config sets it)".to_string(),
            };
            writeln!(
                output,
                "metrics: {} — {layer}",
                if on { "on" } else { "off" }
            )
            .map_err(|e| e.to_string())
        }
        Some(on) => {
            if global {
                let target = global_config_path().ok_or("no HOME for the global config")?;
                write_metrics(&target, on)?;
                writeln!(
                    output,
                    "metrics: {} — wrote {}",
                    if on { "on" } else { "off" },
                    target.display()
                )
                .map_err(|e| e.to_string())?;
                note_repo_override(output, true, Some(on), root.as_deref())
            } else {
                let target = repo_config_path(
                    &root.ok_or("no repository root here — pass --global or run inside a repo")?,
                );
                write_metrics(&target, on)?;
                writeln!(
                    output,
                    "metrics: {} — wrote {}",
                    if on { "on" } else { "off" },
                    target.display()
                )
                .map_err(|e| e.to_string())
            }
        }
    }
}

/// Printed when `enforce` is set, so a success message is never read as
/// enforcement: the guard that acted on it was retired.
const ENFORCE_SCOPE_NOTE: &str = "note: no hook reads the policy any more; `pixel run-hook guard` is retired, so enforce changes no agent's native retrieval";

/// `pixel config policy [advisory|enforce|off] [--global] [--json]`: without
/// a value, report the effective policy and the layer that set it; with one,
/// persist it to the chosen layer. `--json` is the Pi extension's
/// machine-readable form, so it never has to parse the prose.
pub fn run_policy(
    path: &Path,
    global: bool,
    value: Option<PolicyMode>,
    json: bool,
) -> Result<(), String> {
    let root = crate::discover_root(path).ok();
    match value {
        None => {
            let resolution = policy_resolution(root.as_deref());
            if json {
                let mut report = json!({
                    "policy": resolution.mode.as_str(),
                    "source": resolution.source_name(),
                });
                if let Some(file) = resolution.file.as_deref() {
                    report["file"] = json!(file.display().to_string());
                }
                println!(
                    "{}",
                    serde_json::to_string(&report).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "policy: {} — {}",
                    resolution.mode.as_str(),
                    resolution.layer()
                );
            }
            Ok(())
        }
        Some(mode) => {
            let target = if global {
                global_config_path().ok_or("no HOME for the global config")?
            } else {
                repo_config_path(
                    &root.ok_or("no repository root here — pass --global or run inside a repo")?,
                )
            };
            write_doc(&target, |doc| {
                doc["policy"] = Value::String(mode.as_str().to_string());
            })?;
            println!("policy: {} — wrote {}", mode.as_str(), target.display());
            if mode == PolicyMode::Enforce {
                println!("{ENFORCE_SCOPE_NOTE}");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_header_hint_matches_the_input_mode() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        write(&path, "metrics: 'on'\n");
        let mut keyed = Vec::new();
        // q cancels at the first question; the header is already rendered.
        setup_with_keys(
            &path,
            &mut std::io::Cursor::new("q\n"),
            &mut keyed,
            false,
            &KeyReader::forced(true),
        )
        .unwrap();
        assert!(
            String::from_utf8(keyed.clone())
                .unwrap()
                .contains("←/→ pick · Enter confirms"),
            "{keyed:?}"
        );
        let mut lines = Vec::new();
        setup_with_keys(
            &path,
            &mut std::io::Cursor::new("q\n"),
            &mut lines,
            false,
            &KeyReader::inert(),
        )
        .unwrap();
        assert!(
            String::from_utf8(lines.clone())
                .unwrap()
                .contains("Enter keeps the shown value"),
            "{lines:?}"
        );
    }

    use super::*;

    #[test]
    fn failed_install_should_only_revert_a_new_classify_opt_in() {
        for (was_enabled, install_succeeds, expected_enabled) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let home = HomeGuard::set();
            let path = home.0.join("config.yaml");
            write(
                &path,
                &json!({"metrics":"on", "classify":{"enabled":was_enabled}, "future":42})
                    .to_string(),
            );
            let mut calls = 0;
            let result = setup_with_install(
                &path,
                &mut std::io::Cursor::new("n\n\n\n\n\ny\ny\n"),
                &mut Vec::new(),
                false,
                &KeyReader::inert(),
                |_, _| {
                    calls += 1;
                    // A partially completed install can already have written credentials.
                    write_doc(&path, |doc| {
                        doc["remote_keys"] = json!({"openrouter":"retained-secret"});
                        doc["classify"]["engine"] = json!("remote");
                    })?;
                    if install_succeeds {
                        Ok(())
                    } else {
                        Err("model download failed".into())
                    }
                },
                |_, _| Ok(()),
            );
            assert_eq!(calls, 1);
            assert_eq!(
                result,
                if install_succeeds {
                    Ok(true)
                } else {
                    Err("model download failed".into())
                }
            );
            assert_eq!(
                crate::config_file::load(&path).unwrap(),
                json!({
                    "metrics":"off", "daemon_auto_start":true, "task_context":true,
                    "task_boundary":true, "policy":"advisory",
                    "classify":{"enabled":expected_enabled, "engine":"remote"},
                    "future":42, "remote_keys":{"openrouter":"retained-secret"}
                })
            );
        }
    }

    #[test]
    fn web_search_failure_should_not_block_the_classify_installer() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        write(&path, "metrics: 'on'\nclassify: {enabled: true}\n");
        let mut classify_runs = 0;
        let error = setup_with_install(
            &path,
            &mut std::io::Cursor::new("n\n\n\n\n\ny\ny\n"),
            &mut Vec::new(),
            false,
            &KeyReader::inert(),
            |_, _| {
                classify_runs += 1;
                Ok(())
            },
            |_, _| Err("could not ask the web search provider".into()),
        )
        .unwrap_err();
        assert_eq!(
            classify_runs, 1,
            "a web-search failure must not keep the classify installer from running"
        );
        assert_eq!(error, "could not ask the web search provider");
    }

    #[test]
    fn setup_should_not_install_after_cancellation_or_disabling_classify() {
        for (answers, enabled, metrics, saved_expected) in [
            ("q\ny\n", true, "on", false),
            ("n\n\n\n\n\nn\ny\n", false, "off", true),
        ] {
            let home = HomeGuard::set();
            let path = home.0.join("config.yaml");
            write(&path, "metrics: 'on'\nclassify: {enabled: true}\n");
            let saved = setup_with_install(
                &path,
                &mut std::io::Cursor::new(answers),
                &mut Vec::new(),
                false,
                &KeyReader::inert(),
                |_, _| {
                    panic!("cancelled or disabled setup must never invoke the classify installer")
                },
                |_, _| Ok(()),
            )
            .unwrap();
            assert_eq!(saved, saved_expected, "answers: {answers}");
            let doc = crate::config_file::load(&path).unwrap();
            assert_eq!(doc["classify"]["enabled"], enabled);
            assert_eq!(doc["metrics"], metrics);
        }
    }

    #[test]
    fn rollback_failure_should_report_both_the_install_and_storage_errors() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let error = setup_with_install(
            &path,
            &mut std::io::Cursor::new("\n\n\n\n\ny\ny\n"),
            &mut Vec::new(),
            false,
            &KeyReader::inert(),
            |_, _| {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
                Err("model download failed".into())
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.starts_with("model download failed; could not disable classification: cannot read configuration "), "{error}");
    }

    /// The 0.6.1 bug report: setup saved `metrics: "on"` globally while a
    /// legacy repo `config.json` still carried `metrics: "off"`, so every
    /// launch in that repository hid the footer and the answer looked ignored.
    #[test]
    fn repo_override_note_should_name_the_winning_repo_layer() {
        let home = HomeGuard::set();
        let repo = home.0.join("repo");
        let legacy = repo.join(".pixel").join("config.json");
        write(&legacy, &json!({"metrics":"off"}).to_string());
        let mut out = Vec::new();
        note_repo_override(&mut out, true, Some(true), Some(&repo)).unwrap();
        let note = String::from_utf8(out).unwrap();
        assert_eq!(
            note,
            format!(
                "note: {} sets metrics: off and wins in this repository — \
                 run `pixel config metrics on` here to apply this answer\n",
                legacy.display()
            )
        );
    }

    #[test]
    fn repo_override_note_should_stay_silent_unless_a_repo_layer_contradicts() {
        let home = HomeGuard::set();
        let repo = home.0.join("repo");
        let mut out = Vec::new();

        // No repository under the working directory.
        note_repo_override(&mut out, true, Some(true), None).unwrap();
        assert!(out.is_empty());

        // No repo-level metrics setting.
        write(
            &repo.join(".pixel").join("config.yaml"),
            "policy: enforce\n",
        );
        note_repo_override(&mut out, true, Some(true), Some(&repo)).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        // The repo layer agrees with the saved answer.
        write(
            &repo.join(".pixel").join("config.yaml"),
            "metrics: \"on\"\n",
        );
        note_repo_override(&mut out, true, Some(true), Some(&repo)).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        // The settings were never saved (cancelled setup).
        write(
            &repo.join(".pixel").join("config.yaml"),
            "metrics: \"off\"\n",
        );
        note_repo_override(&mut out, false, Some(true), Some(&repo)).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        // The global layer does not pronounce itself.
        note_repo_override(&mut out, true, None, Some(&repo)).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn config_metrics_on_global_should_flag_a_contradicting_repo_layer() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved_home = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        write(
            &repo.join(".pixel").join("config.json"),
            &json!({"metrics":"off"}).to_string(),
        );

        let mut out = Vec::new();
        run_metrics_with(&mut out, &repo, true, Some(true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("metrics: on — wrote"), "{text}");
        assert!(
            text.contains("sets metrics: off and wins in this repository"),
            "{text}"
        );

        // The repo layer itself was left untouched: the note only reports.
        assert_eq!(
            crate::config_file::load(&repo.join(".pixel").join("config.json")).unwrap(),
            json!({"metrics":"off"})
        );
        restore_home(saved_home);
    }

    #[test]
    fn config_metrics_on_repo_should_not_flag_the_layer_it_just_wrote() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved_home = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        write(
            &repo.join(".pixel").join("config.json"),
            &json!({"metrics":"off"}).to_string(),
        );

        let mut out = Vec::new();
        run_metrics_with(&mut out, &repo, false, Some(true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("metrics: on — wrote"), "{text}");
        assert!(!text.contains("wins in this repository"), "{text}");
        // The repo write targets the legacy json when no yaml exists, and no
        // note is printed: the layer just written agrees by construction.
        assert_eq!(
            crate::config_file::load(&repo.join(".pixel").join("config.json")).unwrap(),
            json!({"metrics":"on"})
        );
        restore_home(saved_home);
    }

    #[test]
    fn setup_should_preserve_unrelated_changes_made_while_prompting() {
        struct UpdatingOutput<'a> {
            path: &'a Path,
            updated: bool,
        }
        impl Write for UpdatingOutput<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.updated {
                    // The first prompt is printed after setup has loaded its draft.
                    write_doc(self.path, |doc| {
                        doc["remote_keys"] = json!({"openrouter":"new-secret"});
                        doc["classify"] = json!({"engine":"remote", "enabled":true});
                        doc["future"] = json!(42);
                    })
                    .map_err(std::io::Error::other)?;
                    self.updated = true;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        write(&path, "remote_keys: {openrouter: old-secret}\n");
        let mut output = UpdatingOutput {
            path: &path,
            updated: false,
        };
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("n\nn\nn\nn\nn\nn\ny\n"),
                &mut output,
                false,
            )
            .unwrap()
        );
        assert!(output.updated);
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "metrics":"off", "daemon_auto_start":false, "task_context":false,
                "task_boundary":false, "policy":"advisory",
                "classify":{"engine":"remote", "enabled":false},
                "remote_keys":{"openrouter":"new-secret"}, "future":42
            })
        );
    }

    #[test]
    fn setup_should_save_choices_preserve_secrets_and_allow_cancellation() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let original = "# personal comment\nremote_keys: {openrouter: hidden-secret}\nclassify: {engine: remote}\nunknown: 42\n";
        write(&path, original);
        for answers in ["q\n", "", "n\nn\nn\nn\nn\nn\nn\n", "n\nn\nn\nn\nn\nn\n"] {
            assert!(
                !setup_with(
                    &path,
                    &mut std::io::Cursor::new(answers),
                    &mut Vec::new(),
                    false
                )
                .unwrap()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        let mut output = Vec::new();
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("n\nn\nn\nn\nn\nn\ny\n"),
                &mut output,
                false,
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "remote_keys": {"openrouter":"hidden-secret"}, "unknown":42,
                "metrics":"off", "daemon_auto_start":false, "task_context":false,
                "task_boundary":false, "policy":"advisory",
                "classify":{"engine":"remote", "enabled":false}
            })
        );
        let output = String::from_utf8(output).unwrap();
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("  classify") && line.ends_with("disabled")),
            "{output}"
        );
        // The booleans read as words in the review rows.
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("  daemon auto-start") && line.ends_with("no")),
            "{output}"
        );
        assert!(output.contains("Saved."));
        assert!(!output.contains("hidden-secret"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# personal comment")
        );
        // Enter keeps saved values; explicit yes re-enables every switch.
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\n\ny\n"),
                &mut Vec::new(),
                false,
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap()["classify"]["enabled"],
            false
        );
        // Enter keeps the saved policy too, rather than defaulting the prompt.
        assert_eq!(
            crate::config_file::load(&path).unwrap()["policy"],
            "advisory"
        );
        let mut output = Vec::new();
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("y\ny\ny\ny\ny\ny\ny\n"),
                &mut output,
                false,
            )
            .unwrap()
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("  classify") && line.ends_with("enabled"))
        );
        let doc = crate::config_file::load(&path).unwrap();
        assert_eq!(doc["metrics"], "on");
        for key in ["daemon_auto_start", "task_context", "task_boundary"] {
            assert_eq!(doc[key], true);
        }
        assert_eq!(doc["policy"], "enforce");
        assert_eq!(doc["classify"]["enabled"], true);
    }

    #[test]
    fn pressing_enter_at_the_save_prompt_writes_the_answers() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        write(&path, "# template\n");
        // The header promises Enter keeps the shown value; the save question
        // must honour that too, writing the answers instead of discarding a
        // whole session of y/n responses.
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\n\n\n"),
                &mut Vec::new(),
                false
            )
            .unwrap()
        );
        let saved = crate::config_file::load(&path).unwrap();
        assert_eq!(saved["metrics"], "on");
        assert_eq!(saved["daemon_auto_start"], true);
        assert_eq!(saved["task_context"], true);
        assert_eq!(saved["task_boundary"], true);
        assert_eq!(saved["policy"], "advisory");
    }

    #[test]
    fn setup_should_offer_classify_yes_by_default_to_a_new_user() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\n\ny\n"),
                &mut Vec::new(),
                false,
            )
            .unwrap()
        );
        // Enter on the classify question takes the shown default: yes. The
        // runtime kill switch (`classify_enabled_in` on an unset key) stays
        // false — this saved value is what turns it on.
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "metrics":"on", "daemon_auto_start":true, "task_context":true,
                "task_boundary":true, "policy":"advisory", "classify":{"enabled":true}
            })
        );
        // A saved opt-out is respected: Enter keeps classify off rather
        // than re-asking the user into it.
        write(&path, "classify: {enabled: false}\n");
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\n\ny\n"),
                &mut Vec::new(),
                false,
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap()["classify"]["enabled"],
            false
        );
        // A non-boolean stored value is an error, not a silent default.
        write(&path, "classify: {enabled: \"false\"}\n");
        assert_eq!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\n\ny\n"),
                &mut Vec::new(),
                false,
            )
            .unwrap_err(),
            "classify.enabled must be true or false"
        );
    }

    #[test]
    fn the_classify_explanation_names_jev_and_the_default_shows_yes() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        // Five noes answer the five questions before classify; q then
        // cancels at the classify question itself, after it was printed.
        let mut output = Vec::new();
        setup_with(
            &path,
            &mut std::io::Cursor::new("n\nn\nn\nn\nn\nq\n"),
            &mut output,
            true,
        )
        .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(
            rendered.contains("optional AI classification, separate from code search."),
            "{rendered}"
        );
        assert!(
            rendered.contains("Local runs offline after setup."),
            "{rendered}"
        );
        assert!(!rendered.contains("TypeSafe's Jev"), "{rendered}");
        assert!(!rendered.contains("0.738"), "{rendered}");
        assert!(rendered.contains("pixel classify"), "{rendered}");
        assert!(rendered.contains("\x1b[1;32mPixel setup"), "{rendered}");
        assert!(
            rendered.contains("Allow pixel classify? [Y/n] >"),
            "{rendered}"
        );
        let mut output = Vec::new();
        setup_with(
            &path,
            &mut std::io::Cursor::new("n\nn\nn\nn\nn\nq\n"),
            &mut output,
            false,
        )
        .unwrap();
        assert!(!String::from_utf8(output).unwrap().contains('\x1b'));
    }

    #[test]
    fn prompts_should_keep_defaults_retry_invalid_answers_and_cancel_on_eof() {
        for (answer, current, expected) in [
            ("\n", true, Some(true)),
            ("\n", false, Some(false)),
            ("YES\n", false, Some(true)),
            ("no\n", true, Some(false)),
            ("q\ny\n", true, None),
            ("", true, None),
            ("invalid\nn\n", true, Some(false)),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                ask_bool(
                    &mut std::io::Cursor::new(answer),
                    &mut output,
                    "Choice",
                    current,
                    &KeyReader::inert(),
                    false
                )
                .unwrap(),
                expected
            );
            assert!(String::from_utf8(output).unwrap().contains("Choice"));
        }
    }

    #[test]
    fn classify_switch_should_default_off_and_reject_non_boolean_values() {
        for (doc, expected) in [
            (json!({}), false),
            (json!({"classify":{"engine":"remote"}}), false),
            (json!({"classify":{"enabled":true}}), true),
            (json!({"classify":{"enabled":false}}), false),
        ] {
            assert_eq!(classify_enabled_in(&doc).unwrap(), expected);
        }
        assert_eq!(
            classify_enabled_in(&json!({"classify":{"enabled":"false"}})).unwrap_err(),
            "classify.enabled must be true or false"
        );
    }

    #[test]
    fn fresh_template_should_have_no_active_options_and_use_the_requested_root() {
        let home = HomeGuard::set();
        let path = ensure_template(Some(&home.0)).unwrap();
        assert_eq!(path, home.0.join(".pixel/config.yaml"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# metrics:"));
        assert!(
            text.lines()
                .all(|line| line.trim().is_empty() || line.starts_with('#')),
            "uncommenting an example must not conflict with an active empty mapping: {text}"
        );
        assert_eq!(crate::config_file::load(&path).unwrap(), json!({}));
        ensure_template(Some(&home.0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn render_should_reject_a_concurrent_change_instead_of_silently_losing_values() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let before = json!({"metrics":"on"});
        write(&path, "metrics: 'off'\n");
        let error = crate::config_file::render(&path, &before, &before).unwrap_err();
        assert!(error.contains("file unchanged"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "metrics: 'off'\n");
    }

    #[test]
    fn yaml_should_drive_all_existing_readers_after_migration() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let legacy = home.0.join(".pixel/config.json");
        write(
            &legacy,
            r#"{"metrics":"off","classify":{"engine":"remote","remote_preset":"deepseek","ollaya":{"base":"http://localhost:11435","argv":["ollaya","serve"]}},"remote_keys":{"deepseek":"key"}}"#,
        );
        let yaml = ensure_template(None).unwrap();
        write(&legacy, "{}");
        assert!(!metrics_enabled(None));
        assert_eq!(classify_engine().as_deref(), Some("remote"));
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Deepseek)
        );
        assert_eq!(
            ollaya_launch(),
            Some(json!({"base":"http://localhost:11435","argv":["ollaya","serve"]}))
        );
        assert_eq!(
            remote_key(crate::decide_remote::Preset::Deepseek).as_deref(),
            Some("key")
        );
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        assert_eq!(crate::config_file::load(&yaml).unwrap()["metrics"], "off");
        restore_home(saved);
    }

    #[test]
    fn daemon_start_should_be_disabled_by_repo_yaml_without_an_environment_switch() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        write(
            &home.0.join(".pixel/config.yaml"),
            "daemon_auto_start: false\n",
        );
        let saved = std::env::var_os("PIXEL_DAEMON_AUTO_START");
        // SAFETY: process-wide environment access is serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var("PIXEL_DAEMON_AUTO_START");
        }
        assert!(matches!(
            crate::auto_start_daemon(&home.0, &crate::Request::Status {}),
            Err(crate::InProcessReason::AutoStartDisabled)
        ));
        // SAFETY: same lock as above.
        unsafe {
            if let Some(value) = saved {
                std::env::set_var("PIXEL_DAEMON_AUTO_START", value);
            }
        }
    }

    #[test]
    fn yaml_should_preserve_comments_and_unknown_values_when_commands_update_settings() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let original = "# my settings\nmetrics: \"on\" # keep footer note\nclassify:\n  # provider choice\n  engine: auto\n  custom: 42\nremote_keys:\n  ollama: old\n  openrouter: keep\n";
        write(&path, original);
        write_doc(&path, |doc| {
            doc["metrics"] = json!("off");
            doc["classify"]["engine"] = json!("remote");
            doc["remote_keys"].as_object_mut().unwrap().remove("ollama");
            doc["remote_keys"]["deepseek"] = json!("special: # ' \"\nsecret");
        })
        .unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        for comment in ["# my settings", "# keep footer note", "# provider choice"] {
            assert!(result.contains(comment), "{result}");
        }
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "metrics": "off", "classify": {"engine": "remote", "custom": 42},
                "remote_keys": {"openrouter": "keep", "deepseek": "special: # ' \"\nsecret"}
            })
        );
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn migration_should_preserve_legacy_values_and_leave_existing_yaml_alone() {
        let home = HomeGuard::set();
        let yaml = home.0.join("config.yaml");
        let legacy = home.0.join("config.json");
        let original = r#"{"metrics":"off","remote_keys":{"ollama":"secret"},"future":[1,2]}"#;
        write(&legacy, original);
        assert_eq!(crate::config_file::preferred_path(&home.0), legacy);
        crate::config_file::ensure(&yaml).unwrap();
        assert_eq!(
            crate::config_file::load(&yaml).unwrap(),
            serde_json::from_str::<Value>(original).unwrap()
        );
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);
        assert_eq!(crate::config_file::preferred_path(&home.0), yaml);
        let contents = std::fs::read_to_string(&yaml).unwrap();
        assert!(contents.contains("# metrics:"));
        write(&legacy, "{}");
        crate::config_file::ensure(&yaml).unwrap();
        assert_eq!(std::fs::read_to_string(&yaml).unwrap(), contents);
        #[cfg(unix)]
        assert_eq!(mode(&yaml), 0o600);
    }

    #[test]
    fn invalid_config_should_fail_without_overwriting_or_echoing_secrets() {
        let home = HomeGuard::set();
        for (extension, contents) in [
            ("yaml", "remote_keys: [secret-invalid"),
            ("json", "{\"secret-invalid\":"),
            ("yaml", "- secret-invalid"),
        ] {
            let path = home.0.join(format!("config.{extension}"));
            write(&path, contents);
            let err = write_metrics(&path, false).unwrap_err();
            assert!(err.contains("configuration"));
            assert!(!err.contains("secret-invalid"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
        }
    }

    #[test]
    fn invalid_feature_values_should_fall_through_without_claiming_their_source() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        let global = home.0.join(".pixel/config.yaml");
        let local = repo.join(".pixel/config.yaml");
        let env = format!("PIXEL_TEST_INVALID_FEATURE_{}", std::process::id());
        for (key, _) in FEATURES {
            for invalid in ["'off'", "'no'", "0", "null", "{}"] {
                write(&global, &format!("{key}: false\n"));
                write(&local, &format!("{key}: {invalid}\n"));
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (false, global.display().to_string()),
                    "invalid repository {key}={invalid} must not hide the global opt-out"
                );
                write(&global, &format!("{key}: {invalid}\n"));
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (true, "default".into()),
                    "invalid values at both layers must leave the default as the source"
                );
            }
        }
        restore_home(saved);
    }

    #[test]
    fn features_should_resolve_environment_then_repo_then_global_then_default() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        let env = format!("PIXEL_TEST_FEATURE_{}", std::process::id());
        for (key, _) in FEATURES {
            let global = home.0.join(".pixel/config.yaml");
            let local = repo.join(".pixel/config.yaml");
            write(&global, "{}");
            write(&local, "{}");
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (true, "default".into())
            );
            write(&global, &format!("{key}: false\n"));
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (false, global.display().to_string())
            );
            write(&local, &format!("{key}: true\n"));
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (true, local.display().to_string())
            );
            for (value, enabled) in [
                ("0", false),
                ("off", false),
                ("false", false),
                ("1", true),
                ("", true),
                ("no", true),
            ] {
                // SAFETY: this test owns the unique environment name under ENV_LOCK.
                unsafe {
                    std::env::set_var(&env, value);
                }
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (enabled, env.clone())
                );
                assert_eq!(feature_enabled(Some(&repo), key, &env), enabled);
            }
            // SAFETY: same lock and unique variable as above.
            unsafe {
                std::env::remove_var(&env);
            }
        }
        restore_home(saved);
    }

    #[test]
    fn template_should_be_inert_and_validation_should_reject_wrong_known_types() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        crate::config_file::ensure(&path).unwrap();
        assert_eq!(crate::config_file::load(&path).unwrap(), json!({}));
        validate(&path).unwrap();
        for invalid in [
            "metrics: false",
            "task_context: 'off'",
            "classify: []",
            "classify: {engine: invalid}",
            "classify: {remote_preset: invalid}",
            "web_search: []",
            "web_search: {searxng_url: 12}",
            "web_search: {searxng_url: ''}",
        ] {
            write(&path, invalid);
            assert!(validate(&path).is_err(), "{invalid}");
        }
        write(
            &path,
            "metrics: 'on'\nclassify: {engine: local, remote_preset: deepseek}\ntask_context: true\n\
             web_search: {searxng_url: https://sx.test}\nremote_keys: {perplexity: pplx}",
        );
        validate(&path).unwrap();
    }

    #[test]
    fn removing_the_searxng_url_leaves_a_config_validated_by_validate() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let cfg = home.0.join(".pixel/config.yaml");

        set_web_search_searxng_url("https://sx.test").unwrap();
        validate(&cfg).unwrap();
        remove_web_search_searxng_url().unwrap();
        assert!(
            web_search_searxng_url().is_none(),
            "field removed, not emptied"
        );
        // `validate` accepts the removed state — an empty `searxng_url`
        // would be rejected, and would stall every later `pixel config setup`.
        validate(&cfg).unwrap();
        write(&cfg, "web_search: {searxng_url: ''}");
        assert!(validate(&cfg).is_err(), "empty searxng_url stays rejected");

        restore_home(saved);
    }

    #[test]
    fn web_search_settings_roundtrip_the_url_and_perplexity_key() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        assert!(web_search_searxng_url().is_none(), "nothing stored → unset");
        assert!(
            web_search_perplexity_key().is_none(),
            "nothing stored → unset"
        );
        set_web_search_searxng_url("https://sx.test").unwrap();
        set_web_search_perplexity_key("pplx-secret").unwrap();
        assert_eq!(web_search_searxng_url().as_deref(), Some("https://sx.test"));
        assert_eq!(web_search_perplexity_key().as_deref(), Some("pplx-secret"));

        let cfg: Value = serde_saphyr::from_str(
            &std::fs::read_to_string(home.0.join(".pixel/config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["web_search"]["searxng_url"], "https://sx.test");
        assert_eq!(cfg["remote_keys"]["perplexity"], "pplx-secret");

        restore_home(saved);
    }

    #[test]
    fn setting_the_perplexity_key_preserves_existing_remote_keys() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        write(
            &home.0.join(".pixel/config.yaml"),
            "remote_keys: {openrouter: sk-existing}\n",
        );
        set_web_search_perplexity_key("pplx-secret").unwrap();

        assert_eq!(web_search_perplexity_key().as_deref(), Some("pplx-secret"));
        let cfg: Value = serde_saphyr::from_str(
            &std::fs::read_to_string(home.0.join(".pixel/config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["remote_keys"]["openrouter"], "sk-existing");
        assert_eq!(cfg["remote_keys"]["perplexity"], "pplx-secret");

        restore_home(saved);
    }

    /// A section that somehow holds a scalar (a hand-edited config) must be
    /// repaired by the setters: the create-if-missing guard builds a fresh
    /// object instead of indexing into the non-object, which would panic.
    #[test]
    fn web_search_setters_recreate_a_section_that_is_not_an_object() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        write(
            &home.0.join(".pixel/config.yaml"),
            "web_search: 7\nremote_keys: false\n",
        );
        set_web_search_searxng_url("https://sx.test").unwrap();
        set_web_search_perplexity_key("pplx-secret").unwrap();
        assert_eq!(web_search_searxng_url().as_deref(), Some("https://sx.test"));
        assert_eq!(web_search_perplexity_key().as_deref(), Some("pplx-secret"));
        restore_home(saved);
    }

    struct HomeGuard(PathBuf);

    impl HomeGuard {
        fn set() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pixel-config-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            Self(dir)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn home_env() -> Option<std::ffi::OsString> {
        std::env::var_os("HOME")
    }

    fn point_home(dir: &Path) {
        // SAFETY: under crate::ENV_LOCK in tests only.
        unsafe { std::env::set_var("HOME", dir) };
    }

    fn restore_home(saved: Option<std::ffi::OsString>) {
        // SAFETY: under crate::ENV_LOCK in tests only.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn write(path: &Path, doc: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, doc).unwrap();
    }

    #[test]
    fn unset_layers_default_on_and_nearest_scope_wins() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();

        assert!(metrics_enabled(Some(&repo)), "unset defaults on");

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(!metrics_enabled(Some(&repo)), "repo off wins");

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"on\"}");
        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(metrics_enabled(Some(&repo)), "repo on overrides global off");
        assert!(
            !metrics_enabled(Some(&repo.join("no-config-here"))),
            "global off applies where no repo layer speaks"
        );

        restore_home(saved);
    }

    #[test]
    fn malformed_layers_are_skipped_not_fatal() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        write(&repo.join(".pixel/config.json"), "not json");
        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(
            !metrics_enabled(Some(&repo)),
            "malformed repo falls through to global"
        );

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"loud\"}");
        assert!(
            !metrics_enabled(Some(&repo)),
            "unknown value is not a setting"
        );
        write(
            &home.0.join(".pixel/config.json"),
            "{\"metrics\": \"off\", \"future\": 1}",
        );
        assert!(!metrics_enabled(Some(&repo)));

        restore_home(saved);
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn every_config_write_leaves_the_stored_keys_owner_only() {
        let home = HomeGuard::set();
        let cfg = home.0.join(".pixel/config.json");
        write(&cfg, "{\"remote_keys\": {\"openrouter\": \"sk-secret\"}}");
        // A world-readable leftover tmp from a crashed write must not lend
        // its mode to the next one.
        let tmp = cfg.with_file_name(format!("config.json.{}.tmp", std::process::id()));
        write(&tmp, "{}");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_metrics(&cfg, false).unwrap();
        assert_eq!(
            mode(&cfg),
            0o600,
            "an unrelated write keeps the keys private"
        );
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            text.contains("sk-secret") && text.contains("\"off\""),
            "{text}"
        );
    }

    #[test]
    fn a_dash_reads_the_key_from_stdin_and_anything_else_is_the_key() {
        let mut stdin = std::io::Cursor::new(b"sk-from-stdin\nignored\n".to_vec());
        assert_eq!(
            key_from_arg(Some("-".into()), &mut stdin),
            Ok(Some("sk-from-stdin".into()))
        );
        let mut untouched = std::io::Cursor::new(b"never read\n".to_vec());
        assert_eq!(
            key_from_arg(Some("sk-inline".into()), &mut untouched),
            Ok(Some("sk-inline".into()))
        );
        assert_eq!(key_from_arg(None, &mut untouched), Ok(None));
        assert_eq!(untouched.position(), 0, "stdin is read only for `-`");
    }

    #[test]
    fn write_metrics_preserves_unknown_keys_and_roundtrips() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();
        let cfg = repo.join(".pixel/config.json");
        write(&cfg, "{\"future\": {\"nested\": true}}");

        write_metrics(&cfg, false).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["metrics"], "off");
        assert_eq!(doc["future"]["nested"], true);
        assert!(!metrics_enabled(Some(&repo)));

        write_metrics(&cfg, true).unwrap();
        assert!(metrics_enabled(Some(&repo)));
        assert!(
            !cfg.with_extension("json.tmp").exists(),
            "temp file is renamed away"
        );

        restore_home(saved);
    }

    #[test]
    fn resolution_names_the_layer_that_set_the_value() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();

        assert_eq!(
            metrics_resolution(Some(&repo)),
            (true, Source::Default),
            "nothing set → default on"
        );
        assert!(
            metrics_enabled(None),
            "no repo handle still resolves through the default"
        );

        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert_eq!(
            metrics_resolution(Some(&repo)),
            (false, Source::Global),
            "global off shows up as the global layer"
        );
        assert!(
            !metrics_enabled(None),
            "without a repo the global layer is the answer"
        );

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"on\"}");
        assert_eq!(
            metrics_resolution(Some(&repo)),
            (true, Source::Repo),
            "repo on overrides a global off"
        );

        restore_home(saved);
    }

    #[test]
    fn policy_values_round_trip_and_reject_every_other_spelling() {
        for mode in [PolicyMode::Advisory, PolicyMode::Enforce, PolicyMode::Off] {
            assert_eq!(PolicyMode::parse(mode.as_str()), Some(mode));
        }
        for value in ["", "advisory ", "ADVISORY", "Enforce", "loud", "offf"] {
            assert_eq!(PolicyMode::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn policy_resolution_reads_env_then_repo_then_global_and_labels_each_layer() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let repo_dir = HomeGuard::set();
        let saved_home = home_env();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["PIXEL_POLICY", "PIXEL_TARGETS_GUARD"]
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
        for (name, _) in &saved {
            // SAFETY: under crate::ENV_LOCK in tests only.
            unsafe { std::env::remove_var(name) };
        }
        point_home(&home.0);
        let repo = repo_dir.0.clone();
        let global_path = home.0.join(".pixel/config.yaml");
        let repo_path = repo.join(".pixel/config.yaml");

        let default = policy_resolution(Some(&repo));
        assert_eq!(default.mode, PolicyMode::Advisory);
        assert_eq!(default.source_name(), "default");
        assert_eq!(default.layer(), "default (no config sets it)");
        assert_eq!(default.overview_label(), "default");

        // A value the layer cannot read is not a policy: it falls through
        // to the next layer.
        write(&global_path, "policy: loudly\n");
        write(&repo_path, "policy: null\n");
        assert_eq!(policy_resolution(Some(&repo)).mode, PolicyMode::Advisory);

        // Quoted like every YAML string: a bare `off` is a YAML boolean.
        write(&global_path, "policy: \"off\"\n");
        let global = policy_resolution(Some(&repo));
        assert_eq!(global.mode, PolicyMode::Off);
        assert_eq!(global.source_name(), "global");
        assert_eq!(global.layer(), format!("global {}", global_path.display()));
        assert_eq!(global.overview_label(), global_path.display().to_string());
        assert_eq!(
            policy_resolution(None).mode,
            PolicyMode::Off,
            "no repo handle reads global"
        );

        write(&repo_path, "policy: enforce\n");
        let repo_layer = policy_resolution(Some(&repo));
        assert_eq!(repo_layer.mode, PolicyMode::Enforce);
        assert_eq!(repo_layer.source_name(), "repo");
        assert_eq!(repo_layer.layer(), format!("repo {}", repo_path.display()));

        for (value, expected) in [
            ("Enforce ", PolicyMode::Enforce),
            ("ADVISORY", PolicyMode::Advisory),
            ("nonsense", PolicyMode::Advisory),
        ] {
            // SAFETY: under crate::ENV_LOCK in tests only.
            unsafe { std::env::set_var("PIXEL_POLICY", value) };
            let environment = policy_resolution(Some(&repo));
            assert_eq!(environment.mode, expected, "{value:?}");
            assert_eq!(environment.source_name(), "env");
            assert_eq!(environment.file, None);
            assert_eq!(environment.layer(), "PIXEL_POLICY (environment)");
            assert_eq!(environment.overview_label(), "PIXEL_POLICY");
        }

        // The legacy kill switch outranks an explicit enforce.
        // SAFETY: under crate::ENV_LOCK in tests only.
        unsafe {
            std::env::set_var("PIXEL_POLICY", "enforce");
            std::env::set_var("PIXEL_TARGETS_GUARD", "0");
        }
        let legacy = policy_resolution(Some(&repo));
        assert_eq!(legacy.mode, PolicyMode::Off);
        assert_eq!(legacy.layer(), "PIXEL_TARGETS_GUARD (environment)");
        assert_eq!(legacy.overview_label(), "PIXEL_TARGETS_GUARD");

        for (name, value) in saved {
            // SAFETY: under crate::ENV_LOCK in tests only.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        restore_home(saved_home);
    }

    #[test]
    fn remote_key_roundtrips_masked_and_env_free() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let preset = crate::decide_remote::Preset::Ollama;

        assert!(remote_key(preset).is_none(), "nothing stored → unset");
        run_remote_key(preset, Some("sk-test-secret".to_string()), false).unwrap();
        assert_eq!(remote_key(preset).as_deref(), Some("sk-test-secret"));

        let cfg: Value = serde_saphyr::from_str(
            &std::fs::read_to_string(home.0.join(".pixel/config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["remote_keys"]["ollama"], "sk-test-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.0.join(".pixel/config.yaml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "config holds secrets: {mode:o}");
        }

        run_remote_key(preset, None, true).unwrap();
        assert!(remote_key(preset).is_none(), "clear removes the key");

        restore_home(saved);
    }

    #[test]
    fn remote_key_set_keeps_the_other_presets_keys() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        std::fs::create_dir_all(home.0.join(".pixel")).unwrap();
        std::fs::write(
            home.0.join(".pixel/config.yaml"),
            "remote_keys: {openrouter: sk-existing}\n",
        )
        .unwrap();

        run_remote_key(
            crate::decide_remote::Preset::Ollama,
            Some("sk-new".to_string()),
            false,
        )
        .unwrap();
        let cfg: Value = serde_saphyr::from_str(
            &std::fs::read_to_string(home.0.join(".pixel/config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["remote_keys"]["openrouter"], "sk-existing");
        assert_eq!(cfg["remote_keys"]["ollama"], "sk-new");
        assert!(
            run_remote_key(
                crate::decide_remote::Preset::Ollama,
                Some(String::new()),
                false
            )
            .is_err()
        );

        restore_home(saved);
    }

    #[test]
    fn a_failed_publish_leaves_no_tmp_file_behind() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        // A directory cannot be read as configuration and must remain untouched.
        let dir = home.0.join("target-is-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_metrics(&dir, false).expect_err("directory is not configuration");
        assert!(err.contains("cannot read configuration"), "{err}");
        let leftovers: Vec<_> = std::fs::read_dir(&home.0)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp cleaned up: {leftovers:?}");
    }

    #[test]
    fn ollaya_launch_should_replace_malformed_classify_without_losing_other_settings() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let path = home.0.join(".pixel/config.json");
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        let launch = json!({"base": "http://127.0.0.1:11435", "argv": ["ollaya", "serve"]});
        set_ollaya_launch(&launch).unwrap();
        assert_eq!(ollaya_launch(), Some(launch));
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(stored["metrics"], "off");
        restore_home(saved);
    }

    #[test]
    fn classify_preferences_roundtrip_without_losing_sibling_settings() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let path = home.0.join(".pixel/config.json");
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        assert_eq!(classify_remote_preset(), None);
        set_classify_remote_model(crate::decide_remote::Preset::Deepseek, None, None).unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Deepseek)
        );
        assert_eq!(classify_engine().as_deref(), Some("remote"));
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        set_classify_remote_model(crate::decide_remote::Preset::OpencodeGo, None, None).unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );
        set_classify_engine("local").unwrap();

        let launch = json!({
            "base": "http://127.0.0.1:11435",
            "model": "winnow:e4b",
            "argv": ["ollaya", "serve"],
        });
        set_ollaya_launch(&launch).unwrap();
        assert_eq!(ollaya_launch(), Some(launch));
        assert_eq!(classify_engine().as_deref(), Some("local"));
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );
        set_classify_remote_model(crate::decide_remote::Preset::OpencodeGo, None, None).unwrap();
        set_classify_engine("local").unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );

        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["metrics"], "off");
        assert_eq!(stored["classify"]["engine"], "local");
        assert_eq!(stored["classify"]["ollaya"]["model"], "winnow:e4b");

        restore_home(saved);
    }

    #[test]
    fn remote_preset_should_store_the_model_and_base_and_clear_them_on_switch() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        set_classify_remote_model(
            crate::decide_remote::Preset::Jev,
            Some("jev-latest".to_string()),
            Some("https://jev.example.test"),
        )
        .unwrap();
        assert_eq!(classify_engine().as_deref(), Some("remote"));
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Jev)
        );
        assert_eq!(classify_remote_model().as_deref(), Some("jev-latest"));
        assert_eq!(
            classify_remote_base().as_deref(),
            Some("https://jev.example.test")
        );

        set_classify_remote_model(crate::decide_remote::Preset::Deepseek, None, None).unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Deepseek)
        );
        assert_eq!(classify_remote_model(), None);
        assert_eq!(classify_remote_base(), None);

        restore_home(saved);
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    #[test]
    fn the_picker_choice_tokens_keep_the_same_display_width() {
        // The active and inactive tokens must overwrite each other in place
        // without erasing, so their widths must match exactly.
        let (yes_active, no_inactive) = choice_tokens(true, false);
        let (yes_inactive, no_active) = choice_tokens(false, false);
        for token in [
            yes_active.clone(),
            no_inactive.clone(),
            yes_inactive,
            no_active.clone(),
        ] {
            assert_eq!(token.chars().count(), 3, "{token}");
        }
        assert_eq!(yes_active, "(Y)");
        assert_eq!(no_active, "(n)");
    }

    #[test]
    fn color_on_paints_the_active_choice_and_the_inactive_stays_dim() {
        let (yes_active, _) = choice_tokens(true, true);
        assert_eq!(yes_active, "\x1b[1;32m(Y)\x1b[0m");
        let (_, no_active) = choice_tokens(false, true);
        assert_eq!(no_active, "\x1b[1;33m(n)\x1b[0m");
    }

    #[test]
    fn render_choice_writes_the_question_with_the_active_choice_marked() {
        let mut output = Vec::new();
        render_choice(&mut output, "Choice?", true, "  hint", false).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("Choice? [ (Y) /  n  ]  hint"), "{line}");
        assert!(line.starts_with('\r'), "{line}");

        let mut output = Vec::new();
        render_choice(&mut output, "Choice?", false, "  hint", false).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("Choice? [  Y  / (n) ]  hint"), "{line}");
    }

    #[test]
    fn picker_hint_drops_when_the_line_fills_the_terminal() {
        // At exactly the terminal width the hint is dropped; a single spare
        // column keeps it.
        let cols = crate::select::visible_len("Choice? [ (Y) /  n  ]  hint ");
        assert_eq!(picker_hint("Choice?", "  hint", cols), "");
        assert_eq!(picker_hint("Choice?", "  hint", cols + 1), "  hint");
    }

    #[test]
    fn the_review_rows_line_up_on_one_key_column() {
        let mut output = Vec::new();
        let review = [
            ("metrics", "on".to_string()),
            ("daemon auto-start", "yes".to_string()),
            ("classify", "enabled".to_string()),
        ];
        let key_width = review
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0);
        for (key, value) in &review {
            writeln!(
                output,
                "  {}  {}",
                paint(false, "2", &format!("{key:<key_width$}")),
                value
            )
            .unwrap();
        }
        let rendered = String::from_utf8(output).unwrap();
        let value_column = 2 + key_width + 2;
        for (row, (_, value)) in rendered.lines().zip(review.iter()) {
            assert_eq!(
                row.chars().skip(value_column).collect::<String>(),
                *value,
                "{row}"
            );
        }
    }
}
