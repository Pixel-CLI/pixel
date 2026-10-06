// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The classify-engine install step and configuration.
//!
//! `pixel install` proposes the classify engine — local (the Ollaya decision
//! daemon on this Mac), remote (a hosted LLM behind a key), or Jev
//! (TypeSafe's hosted decision model behind `TYPESAFE_API_KEY`) — with each
//! option's measured accuracy in parentheses. Choosing local runs the
//! auto-setup: the `ollaya` binary installed into a pixel-managed prefix,
//! the recommended model pulled, and a recorded server launch that
//! `pixel classify` auto-starts on demand. Choosing remote prompts for the
//! provider's API key and stores it through the existing
//! `pixel config remote-key` flow; Jev prompts for its key directly. The
//! choice is switchable later with `pixel config classify-engine` and
//! `pixel config remote-key <preset>`.
//!
//! Everything interactive is gated on a TTY: a scripted install (CI, pipes)
//! prints the choice it would have asked as a suggestion and moves on —
//! `pixel config classify-engine <local|remote|jev|auto>` records the answer
//! later. The stored preference is advisory: an explicit `--engine` flag on
//! `pixel classify` always wins.

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

/// Install-time proposal text, with each option's measured accuracy in
/// parentheses (Ollaya's published typed-decisions benchmark for
/// `winnow:e4b`; this repository's own 14-item public coding exam for the
/// remote preset).
pub const LOCAL_LABEL: &str = "Local — Ollaya winnow:e4b on this Mac (offline, $0 per call; 0.722 typed-decisions accuracy vs Jev's 0.738)";
pub const REMOTE_LABEL: &str = "Remote — hosted LLM behind your key (DeepSeek-v4.1-flash: 100% on the 14-item public coding exam; ~$0.0001/call, needs network)";
pub const JEV_LABEL: &str = "Jev — TypeSafe's hosted decision model behind your TYPESAFE_API_KEY (0.738 typed-decisions accuracy, needs network)";
/// What one menu row launches: the local auto-setup, the remote-provider
/// key flow (any chat preset), or the Jev flow (a TypeSafe or OpenCode Go
/// key, then the model the source carries).
#[derive(Clone, Copy, PartialEq)]
enum SetupKind {
    Local,
    Remote,
    Jev,
}

/// The install menu, in display order.
const MENU: &[(&str, SetupKind)] = &[
    (LOCAL_LABEL, SetupKind::Local),
    (REMOTE_LABEL, SetupKind::Remote),
    (JEV_LABEL, SetupKind::Jev),
];

/// Everything Ollaya owns lives under one pixel-managed prefix (binary,
/// model store, installer, server log), never in the global PATH.
const OLLAYA_ROOT: &str = ".local/share/pixel/ollaya";

/// The bundled `pixel-classify` skill — how to shape bounded classify calls
/// and wire them into a harness's routing, guards, and grading — deployed
/// into each configured harness's skills dir as `pixel-classify/SKILL.md`.
const CLASSIFY_SKILL: &str = include_str!("../../pixel-install/assets/pixel-classify-skill.md");
/// The bundled pi extension: `ask_pixel_file_bool/choice/score`,
/// `ask_pixel_files`, `pick_pixel_file` — file-level classify calls whose
/// file text never enters the agent's context. It ships as Pixel's classify
/// Pi package ([`pixel_install::ClassifyPiPackage`]), declared in Pi's
/// settings like the impact package, never copied into Pi's `extensions/`.
const PI_CLASSIFY_EXTENSION: &str = include_str!("../../pixel-install/assets/pi-classify-files.ts");
/// The skills-dir name every harness copy of [`CLASSIFY_SKILL`] deploys to.
const CLASSIFY_SKILL_NAME: &str = "pixel-classify";

/// The stored engine preference, if any: `local`, `remote`, or `auto`.
#[cfg_attr(test, mutants::skip)] // Thin config adapter; policy is tested through injected settings.
pub fn stored_engine() -> Option<String> {
    crate::config_cmd::classify_engine()
}

/// The recorded local-daemon launch, if a local install completed.
#[cfg_attr(test, mutants::skip)] // Thin config adapter; launch validation is tested without user config.
pub fn ollaya_launch() -> Option<Value> {
    crate::config_cmd::ollaya_launch()
}

/// The engine `pixel classify` resolves to: the explicit flag wins; then
/// the stored preference; `auto` (or absent) falls back to local when the
/// server answers, remote otherwise.
pub fn resolve_engine(
    flag: Option<crate::classify::EngineChoice>,
    ollaya_url: String,
    stored: Option<String>,
    reachable: bool,
) -> ResolvedEngine {
    match flag {
        Some(crate::classify::EngineChoice::Ollaya) => {
            return ResolvedEngine::Local { base: ollaya_url };
        }
        Some(crate::classify::EngineChoice::Remote) => return ResolvedEngine::Remote,
        None => {}
    }
    match stored.as_deref() {
        Some("local") => ResolvedEngine::Local { base: local_base() },
        Some("remote") => ResolvedEngine::Remote,
        _ if reachable => ResolvedEngine::Local { base: local_base() },
        _ => ResolvedEngine::Remote,
    }
}

/// The engine a resolved classify run talks to.
pub enum ResolvedEngine {
    Remote,
    Local { base: String },
}

/// Make sure a resolved-local engine has a live daemon: auto-start the
/// recorded launch once and poll (the daemon comes up in seconds). When no
/// launch is recorded there is nothing to start, so this returns `Ok`
/// without polling — the caller's request then fails fast against the
/// closed port rather than stalling for two minutes.
#[cfg_attr(test, mutants::skip)] // Runtime adapter; bounded daemon-start policy is tested by `ensure_local_with`.
pub fn ensure_local(base: &str) -> Result<(), String> {
    let mut reachable = || server_reachable(base);
    let mut start = || auto_start(base);
    let mut sleep = std::thread::sleep;
    ensure_local_with(
        base,
        &mut reachable,
        &mut start,
        &mut sleep,
        240,
        Duration::from_millis(500),
    )
}

fn ensure_local_with(
    base: &str,
    reachable: &mut dyn FnMut() -> bool,
    start: &mut dyn FnMut() -> Result<bool, String>,
    sleep: &mut dyn FnMut(Duration),
    attempts: usize,
    delay: Duration,
) -> Result<(), String> {
    if reachable() {
        return Ok(());
    }
    if !start()? {
        return Ok(());
    }
    for _ in 0..attempts {
        if reachable() {
            return Ok(());
        }
        sleep(delay);
    }
    Err(format!(
        "the ollaya daemon at {base} did not come up within two minutes; check ~/.local/share/pixel/ollaya/server.log"
    ))
}

/// Connect cap of [`server_reachable`]: a daemon on this machine accepts in
/// well under a millisecond, so 250 ms only bounds a dead or filtered address.
const REACHABLE_PROBE: Duration = Duration::from_millis(250);

/// Whether the stored engine preference lets a caller use the local daemon:
/// `remote` rules it out; `local`, `auto`, an unknown value or no preference
/// all resolve to local when it answers (see [`resolve_engine`]).
pub fn local_permitted(stored: Option<&str>) -> bool {
    stored != Some("remote")
}

/// Whether the local Ollaya daemon answers on its base URL (TCP level —
/// enough to distinguish "daemon up" from "not started").
pub fn server_reachable(base: &str) -> bool {
    server_reachable_within(base, REACHABLE_PROBE)
}

/// [`server_reachable`] with the connect cap set by the caller: the prompt
/// hook probes with a tighter one than an interactive `pixel classify`.
pub fn server_reachable_within(base: &str, cap: Duration) -> bool {
    use std::net::TcpStream;
    let authority = base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default();
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let port: u16 = authority
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(80);
    std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, cap).is_ok())
}

/// Parse the interactive numbered answer into a menu index.
fn parse_choice(answer: &str) -> Option<usize> {
    answer.trim().parse::<usize>().ok()?.checked_sub(1)
}

/// The install-time step: propose, then dispatch to the chosen setup. When
/// stdin is not a TTY the proposal is printed as a suggestion and nothing
/// interactive happens.
#[cfg_attr(test, mutants::skip)] // Runtime config adapter; interactive policy is tested by `install_step_with`.
pub fn install_step(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    install_step_with(
        tty,
        stdin,
        stdout,
        stored_engine(),
        setup_local,
        propose_remote_key,
        propose_jev_key,
        propose_classify_helpers,
        &mut crate::select::TermiosRaw::default(),
        std::io::stdin().is_terminal(),
    )
}

#[allow(clippy::too_many_arguments)] // the seams are the point: tests inject each collaborator
fn install_step_with<FLocal, FRemote, FJev, FHelpers>(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
    stored: Option<String>,
    setup_local: FLocal,
    propose_remote_key: FRemote,
    propose_jev_key: FJev,
    propose_helpers: FHelpers,
    raw: &mut dyn crate::select::RawMode,
    stdin_is_terminal: bool,
) -> Result<(), String>
where
    FLocal: FnOnce(&mut dyn std::io::Write) -> Result<(), String>,
    FRemote: FnOnce(&mut dyn BufRead, &mut dyn std::io::Write) -> Result<(), String>,
    FJev: FnOnce(&mut dyn BufRead, &mut dyn std::io::Write) -> Result<(), String>,
    FHelpers: FnOnce(&mut dyn BufRead, &mut dyn std::io::Write) -> Result<(), String>,
{
    if let Some(engine) = stored {
        writeln!(stdout, "classify engine: already configured as {engine:?} (change with `pixel config classify-engine <local|remote|jev|auto>`)")
            .map_err(|e| e.to_string())?;
        // An engine chosen on an earlier install still gets the helpers
        // offer — the proposal itself is a no-op once every file it
        // manages is current.
        return propose_helpers(stdin, stdout);
    }
    let print_menu = |stdout: &mut dyn std::io::Write| -> Result<(), String> {
        for (index, (label, _)) in MENU.iter().enumerate() {
            writeln!(stdout, "  [{}] {label}", index + 1).map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    writeln!(stdout, "Classify engine:").map_err(|e| e.to_string())?;
    if !tty {
        print_menu(stdout)?;
        writeln!(stdout, "classify engine: not configured (non-interactive install) — run `pixel config classify-engine <local|remote|jev>` or re-run `pixel install` in a terminal")
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    // TTY: the arrow picker paints the option rows itself; EOF falls back
    // to the numbered prompt so a piped answer still lands.
    let labels: Vec<&str> = MENU.iter().map(|(label, _)| *label).collect();
    let picked = crate::select::pick(&labels, stdin, stdout, raw, stdin_is_terminal)?;
    let choice = match picked {
        Some(index) => Some(index),
        None => {
            print_menu(stdout)?;
            write!(stdout, "Choice> ").map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            stdin
                .read_line(&mut line)
                .map_err(|e| format!("read choice: {e}"))?;
            parse_choice(&line)
        }
    };
    match choice.and_then(|index| MENU.get(index).map(|(_, kind)| *kind)) {
        Some(SetupKind::Local) => setup_local(stdout),
        Some(SetupKind::Remote) => propose_remote_key(stdin, stdout),
        Some(SetupKind::Jev) => propose_jev_key(stdin, stdout),
        None => {
            writeln!(stdout, "classify engine: skipped — run `pixel config classify-engine <local|remote|jev>` to choose later")
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
    }?;
    // The person accepted a classifier; offer the harness helpers on top.
    propose_helpers(stdin, stdout)
}

/// What a "yes" to the helpers proposal writes: the `pixel-classify` skill
/// into every configured harness's skills dir — the skill is
/// harness-agnostic, not pi-only; [`pixel_install::config::SKILL_ROOTS`] is
/// the single list of roots — and, when pi is set up, Pixel's classify Pi
/// package with its `packages` entry. `.claude` is a target unconditionally
/// (`pixel install` writes its hooks there before this step runs); the rest
/// join only when configured.
struct ClassifyHelpers {
    skills: Vec<PathBuf>,
    pi: Option<pixel_install::ClassifyPiPackage>,
}

impl ClassifyHelpers {
    fn at(home: &Path, pi: Option<pixel_install::ClassifyPiPackage>) -> Self {
        let skill_file = |root: &Path| {
            root.join("skills")
                .join(CLASSIFY_SKILL_NAME)
                .join("SKILL.md")
        };
        let mut skills = vec![skill_file(&home.join(".claude"))];
        for root in pixel_install::config::SKILL_ROOTS {
            let dir = home.join(root);
            if dir.is_dir() {
                skills.push(skill_file(&dir));
            }
        }
        Self { skills, pi }
    }

    fn is_current(&self) -> bool {
        self.skills
            .iter()
            .all(|path| fs::read_to_string(path).is_ok_and(|existing| existing == CLASSIFY_SKILL))
            && self
                .pi
                .as_ref()
                .is_none_or(pixel_install::ClassifyPiPackage::is_current)
    }

    /// Every path the proposal names, in the order it writes them.
    fn listed(&self) -> Vec<String> {
        let mut listed: Vec<String> = self
            .skills
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        if let Some(pi) = &self.pi {
            listed.extend(
                pi.files()
                    .iter()
                    .map(|(path, _)| path.display().to_string()),
            );
            listed.push(format!("{} (packages entry)", pi.settings().display()));
        }
        listed
    }
}

/// Offer the classify helpers after a successful engine setup. The helpers
/// are optional extras, so a write failure or a missing HOME downgrades to
/// a printed note — it must never fail (and roll back) the engine install.
#[cfg_attr(test, mutants::skip)] // HOME adapter; policy is tested by `propose_classify_helpers_at`.
fn propose_classify_helpers(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        writeln!(stdout, "classify helpers: skipped — no HOME").map_err(|e| e.to_string())?;
        return Ok(());
    };
    let pi = pixel_install::ClassifyPiPackage::for_home(&home, PI_CLASSIFY_EXTENSION);
    if let Err(error) = propose_classify_helpers_at(&ClassifyHelpers::at(&home, pi), stdin, stdout)
    {
        writeln!(stdout, "classify helpers: skipped — {error}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The helpers proposal for an explicit set of targets: list what would
/// land, ask, and on a yes write each skill with the usual pixel backup of a
/// differing existing file, then the Pi package. A fully current set
/// short-circuits to "already installed" without prompting, which keeps
/// re-installs quiet.
fn propose_classify_helpers_at(
    helpers: &ClassifyHelpers,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    if helpers.is_current() {
        return writeln!(stdout, "classify helpers: already installed").map_err(|e| e.to_string());
    }
    writeln!(stdout, "Classify helpers:").map_err(|e| e.to_string())?;
    let listed = helpers.listed();
    for path in &listed {
        writeln!(stdout, "  {path}").map_err(|e| e.to_string())?;
    }
    write!(stdout, "Install them? [Y/n]> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read helpers answer: {e}"))?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes") {
        return writeln!(stdout, "classify helpers: skipped").map_err(|e| e.to_string());
    }
    for path in &helpers.skills {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        // A failed backup stops the overwrite: an edited helper is never
        // replaced without its copy.
        pixel_install::config::backup_if_changing(path, CLASSIFY_SKILL.as_bytes())
            .map_err(|e| format!("back up {}: {e}", path.display()))?;
        fs::write(path, CLASSIFY_SKILL).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    if let Some(pi) = &helpers.pi {
        pi.install()?;
    }
    writeln!(
        stdout,
        "classify helpers: installed {} file(s)",
        listed.len()
    )
    .map_err(|e| e.to_string())
}

#[cfg_attr(test, mutants::skip)] // Config adapter; prompt and persistence dispatch are tested with injected storage.
fn propose_remote_key(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    propose_remote_key_with(stdin, stdout, |preset, key, model, base| {
        if let Some(key) = key {
            crate::config_cmd::run_remote_key(preset, Some(key.to_string()), false)?;
        }
        crate::config_cmd::set_classify_remote_model(preset, model, base)
    })
}

/// The remote path's provider list: every chat preset except Jev — Jev has
/// its own top-level option because its key source and model need asking.
fn propose_remote_key_with(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
    store: impl FnOnce(
        crate::decide_remote::Preset,
        Option<&str>,
        Option<String>,
        Option<&str>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    writeln!(
        stdout,
        "Remote providers: openrouter / ollama / deepseek / opencode-go"
    )
    .map_err(|e| e.to_string())?;
    write!(stdout, "Provider [openrouter]> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read provider: {e}"))?;
    let provider = if line.trim().is_empty() {
        "openrouter"
    } else {
        line.trim()
    };
    let Some(preset) = crate::decide_remote::Preset::parse_name(provider) else {
        return Err(format!(
            "unknown provider {provider:?} (openrouter, ollama, deepseek, opencode-go, local)"
        ));
    };
    if matches!(
        preset,
        crate::decide_remote::Preset::Jev | crate::decide_remote::Preset::Local
    ) {
        return Err(format!(
            "{provider} is not a remote chat provider — pick Jev or Local at the top menu"
        ));
    }
    propose_key_for(preset, None, None, None, stdin, stdout, store)
}

/// The Jev path: the key can be a TypeSafe key (api.typesafe.ai) or an
/// OpenCode Go key — the subscription serves Jev through `opencode.ai/zen`.
/// Both carry the two Jev models, so the model menu follows.
#[cfg_attr(test, mutants::skip)] // Config adapter; prompt and persistence dispatch are tested with injected storage.
fn propose_jev_key(stdin: &mut dyn BufRead, stdout: &mut dyn std::io::Write) -> Result<(), String> {
    propose_jev_key_with(stdin, stdout, |preset, key, model, base| {
        if let Some(key) = key {
            crate::config_cmd::run_remote_key(preset, Some(key.to_string()), false)?;
        }
        crate::config_cmd::set_classify_remote_model(preset, model, base)
    })
}

fn propose_jev_key_with(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
    store: impl FnOnce(
        crate::decide_remote::Preset,
        Option<&str>,
        Option<String>,
        Option<&str>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    writeln!(stdout, "Jev API key source:").map_err(|e| e.to_string())?;
    writeln!(
        stdout,
        "  [1] OpenCode Go — the subscription's OPENCODE_API_KEY"
    )
    .map_err(|e| e.to_string())?;
    writeln!(stdout, "  [2] TypeSafe — a direct TYPESAFE_API_KEY").map_err(|e| e.to_string())?;
    write!(stdout, "Source [1]> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read key source: {e}"))?;
    // The two sources publish different catalogs — verified against both
    // APIs: opencode.ai/zen lists jev-1.13 + jev-1.13-free, api.typesafe.ai
    // lists jev-latest + jev-preview.
    let (base, key_var, models) = match line.trim() {
        "" | "1" => (
            Some(OPENCODE_ZEN_BASE),
            "OPENCODE_API_KEY",
            ["jev-1.13-free", "jev-1.13"],
        ),
        "2" => (None, "TYPESAFE_API_KEY", ["jev-latest", "jev-preview"]),
        other => return Err(format!("unknown key source {other:?} (1 or 2)")),
    };
    writeln!(stdout, "Jev model:").map_err(|e| e.to_string())?;
    for (index, model) in models.iter().enumerate() {
        writeln!(stdout, "  [{}] {model}", index + 1).map_err(|e| e.to_string())?;
    }
    write!(stdout, "Model [1]> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read model: {e}"))?;
    let model = match line.trim() {
        "" | "1" => Some(models[0].to_string()),
        "2" => Some(models[1].to_string()),
        custom => Some(custom.to_string()),
    };
    propose_key_for(
        crate::decide_remote::Preset::Jev,
        model,
        base,
        Some(key_var),
        stdin,
        stdout,
        store,
    )
}

/// Where the OpenCode subscription serves Jev: the zen host, which proxies
/// TypeSafe's `/v1/systemone` — verified against the live API (`jev-1.13-free`
/// answers there; `zen/go/v1` chat rejects every jev model).
const OPENCODE_ZEN_BASE: &str = "https://opencode.ai/zen";

fn propose_key_for(
    preset: crate::decide_remote::Preset,
    model: Option<String>,
    base: Option<&str>,
    key_var: Option<&str>,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
    store: impl FnOnce(
        crate::decide_remote::Preset,
        Option<&str>,
        Option<String>,
        Option<&str>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let Some(var) = key_var
        .map(str::to_string)
        .or_else(|| crate::decide_remote::key_env_name(preset, None))
    else {
        return Err(
            "the local preset needs no API key — choose it as the engine instead".to_string(),
        );
    };
    write!(
        stdout,
        "API key (stored in the global Pixel config, never printed)> "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut key = String::new();
    stdin
        .read_line(&mut key)
        .map_err(|e| format!("read key: {e}"))?;
    let key = key.trim();
    store(
        preset,
        if key.is_empty() { None } else { Some(key) },
        model,
        base,
    )?;
    if key.is_empty() {
        writeln!(stdout, "classify engine: remote selected, no key stored — set {var} or run `pixel config remote-key {} -` before classifying", preset.display())
            .map_err(|e| e.to_string())?;
    } else {
        writeln!(
            stdout,
            "classify engine: remote ({}) — key stored",
            preset.display()
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The local auto-setup: download the installer, install the `ollaya`
/// binary into the pixel-managed prefix, pull the recommended model, and
/// record a launch that `pixel classify` auto-starts. Long-running and
/// network-bound; every step is echoed as it starts.
#[cfg_attr(test, mutants::skip)] // System adapter; setup policy is tested against `LocalSetupRuntime`.
pub fn setup_local(stdout: &mut dyn std::io::Write) -> Result<(), String> {
    setup_local_with(&mut SystemLocalSetup, stdout)
}

trait LocalSetupRuntime {
    fn local_root(&mut self) -> Result<PathBuf, String>;
    fn exists(&self, path: &std::path::Path) -> bool;
    /// The unpack support the ollaya installer needs, observed on this
    /// machine: `zstd (command present?)`, the platform, and the parsed
    /// `/etc/os-release` when the platform ships one.
    fn unpack_environment(&self) -> UnpackEnvironment;
    fn run(
        &mut self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<(), String>;
    fn record_launch(&mut self, launch: &Value) -> Result<(), String>;
    fn set_engine(&mut self) -> Result<(), String>;
}

/// What this machine offers for unpacking a `zstd`-compressed artifact.
/// The system adapter reads `/etc/os-release` (Linux) and probes the `zstd`
/// command; the test fake supplies fixed values.
#[derive(Debug, PartialEq, Eq, Clone)]
struct UnpackEnvironment {
    zstd_present: bool,
    os: &'static str,
    os_release: Option<String>,
}

struct SystemLocalSetup;

impl LocalSetupRuntime for SystemLocalSetup {
    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn local_root(&mut self) -> Result<PathBuf, String> {
        local_root()
    }

    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn exists(&self, path: &std::path::Path) -> bool {
        path.exists()
    }

    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn unpack_environment(&self) -> UnpackEnvironment {
        let zstd_present = probe_present("zstd", &["--version"]);
        let os_release = if std::env::consts::OS == "linux" {
            std::fs::read_to_string("/etc/os-release").ok()
        } else {
            None
        };
        UnpackEnvironment {
            zstd_present,
            os: std::env::consts::OS,
            os_release,
        }
    }

    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn run(
        &mut self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<(), String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let env: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        run_env(program, &args, &env)
    }

    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn record_launch(&mut self, launch: &Value) -> Result<(), String> {
        crate::config_cmd::set_ollaya_launch(launch)
    }

    #[cfg_attr(test, mutants::skip)] // System I/O adapter; setup policy is exercised through LocalSetupRuntime.
    fn set_engine(&mut self) -> Result<(), String> {
        crate::config_cmd::set_classify_engine("local")
    }
}

fn setup_local_with(
    runtime: &mut impl LocalSetupRuntime,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    ensure_unpack_support(runtime, stdout)?;
    let root = runtime.local_root()?;
    let root_str = root.to_string_lossy().into_owned();
    let bin = root.join("bin").join("ollaya");
    let models = root.join("models");
    let models_str = models.to_string_lossy().into_owned();

    writeln!(
        stdout,
        "ollaya setup [1/2] install the ollaya binary (https://ollaya.dev/install.sh) → {}",
        bin.display()
    )
    .map_err(|e| e.to_string())?;
    if !runtime.exists(&bin) {
        let installer = root.join("install.sh");
        let installer_str = installer.to_string_lossy().into_owned();
        runtime.run(
            "curl",
            &[
                "-fsSL".to_string(),
                "https://ollaya.dev/install.sh".to_string(),
                "-o".to_string(),
                installer_str.clone(),
            ],
            &[],
        )?;
        runtime.run(
            "sh",
            &[installer_str],
            &[
                ("OLLAYA_INSTALL_DIR".to_string(), root_str.clone()),
                ("OLLAYA_NO_SERVICE".to_string(), "1".to_string()),
            ],
        )?;
        if !runtime.exists(&bin) {
            return Err(format!(
                "the ollaya installer finished but {} is missing",
                bin.display()
            ));
        }
    }

    writeln!(
        stdout,
        "ollaya setup [2/2] pull {} (model weights; runs on Apple-silicon Metal)",
        crate::decide_ollaya::DEFAULT_MODEL
    )
    .map_err(|e| e.to_string())?;
    let bin_str = bin.to_string_lossy().into_owned();
    runtime.run(
        &bin_str,
        &[
            "pull".to_string(),
            crate::decide_ollaya::DEFAULT_MODEL.to_string(),
        ],
        &[("OLLAYA_MODELS".to_string(), models_str.clone())],
    )?;

    let launch = json!({
        "base": crate::decide_ollaya::DEFAULT_BASE,
        "model_name": crate::decide_ollaya::DEFAULT_MODEL,
        "argv": [bin_str, "serve".to_string()],
        "env": {
            "OLLAYA_MODELS": models_str,
            "OLLAYA_HOST": "127.0.0.1:11435",
        },
    });
    runtime.record_launch(&launch)?;
    runtime.set_engine()?;
    writeln!(
        stdout,
        "classify engine: local — `pixel classify` will auto-start the daemon on demand"
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// The ollaya installer unpacks its download with `zstd`, which ollaya does
/// not bundle: whether the command exists depends on the distribution, and
/// a fresh box without it died inside the remote `install.sh` with a bare
/// exit status. Install it before the installer runs — without asking for
/// consent; the operating system may still require its own sudo password. A
/// distribution the table cannot name ends with the remedy in the error,
/// never a cryptic status.
fn ensure_unpack_support(
    runtime: &mut impl LocalSetupRuntime,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    let env = runtime.unpack_environment();
    if env.zstd_present {
        return Ok(());
    }
    let Some(install) = unpack_support_install(&env) else {
        return Err(format!(
            "ollaya setup needs `zstd` to unpack its download, and the \
             preflight has no automatic install for os {os}: install it \
             with the distribution's package manager (apt: `sudo apt-get \
             install -y zstd`; dnf: `sudo dnf install -y zstd`; zypper: \
             `sudo zypper install zstd`; pacman: `sudo pacman -S zstd`; \
             apk: `sudo apk add zstd`) and re-run pixel config setup",
            os = env.os
        ));
    };
    writeln!(
        stdout,
        "ollaya setup needs zstd to unpack its download (not bundled with \
         ollaya; installed per distribution) — installing {}",
        install.join(" ")
    )
    .map_err(|e| e.to_string())?;
    runtime.run(&install[0], &install[1..], &[])?;
    if !runtime.unpack_environment().zstd_present {
        return Err(format!(
            "ollaya setup ran `{}` but zstd is still missing; install it \
             manually (apt: `sudo apt-get install -y zstd`; dnf: `sudo dnf \
             install -y zstd`; zypper: `sudo zypper install zstd`; pacman: \
             `sudo pacman -S zstd`; apk: `sudo apk add zstd`) and re-run \
             pixel config setup",
            install.join(" ")
        ));
    }
    Ok(())
}

fn probe_present(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Family per package manager, as `ID` or `ID_LIKE` names them in
/// `/etc/os-release`; RHEL-era distributions name the CLI `zstd` while the
/// library (`libzstd`) is usually already present.
const UNPACK_INSTALLERS: &[(&[&str], &[&str])] = &[
    (
        &["ubuntu", "debian", "pop", "linuxmint", "raspbian"],
        &["sudo", "apt-get", "install", "-y", "zstd"],
    ),
    (
        &["fedora", "rhel", "centos", "amzn", "rocky", "ol"],
        &["sudo", "dnf", "install", "-y", "zstd"],
    ),
    (
        &[
            "opensuse",
            "opensuse-leap",
            "opensuse-tumbleweed",
            "suse",
            "sles",
        ],
        &["sudo", "zypper", "install", "--non-interactive", "zstd"],
    ),
    (
        &["arch", "manjaro", "endeavouros", "garuda"],
        &["sudo", "pacman", "-S", "--noconfirm", "zstd"],
    ),
    (&["alpine"], &["sudo", "apk", "add", "zstd"]),
];

/// The non-interactive install command for this machine's distribution, or
/// `None` when the preflight cannot name one (`os_release` unparseable, or
/// an os outside the table).
fn unpack_support_install(env: &UnpackEnvironment) -> Option<Vec<String>> {
    match env.os {
        "macos" => Some(vec!["brew".into(), "install".into(), "zstd".into()]),
        "linux" => {
            let release = env.os_release.as_deref()?;
            let id = os_release_field(release, "ID");
            let id_like = os_release_field(release, "ID_LIKE");
            UNPACK_INSTALLERS
                .iter()
                .find(|(families, _)| {
                    families.contains(&id.as_str())
                        || id_like.split(' ').any(|like| families.contains(&like))
                })
                .map(|(_, command)| command.iter().map(ToString::to_string).collect())
        }
        _ => None,
    }
}

/// The unquoted value of a KEY=VALUE line of `/etc/os-release`, or an empty
/// string when absent.
fn os_release_field(release: &str, key: &str) -> String {
    for line in release.lines() {
        if let Some(value) = line.strip_prefix(&format!("{key}=")) {
            return value.trim_matches('"').to_string();
        }
    }
    String::new()
}

/// Spawn the recorded local daemon if it is not already answering. Returns/// whether a spawn happened (the caller polls for reachability itself).
#[cfg_attr(test, mutants::skip)] // Runtime adapter; launch parsing and branching are tested by `auto_start_with`.
pub fn auto_start(base: &str) -> Result<bool, String> {
    auto_start_with(base, server_reachable, ollaya_launch(), |argv, env| {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(local_root()?.join("server.log"))
            .map_err(|e| format!("open server log: {e}"))?;
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        command
            .spawn()
            .map_err(|e| format!("start ollaya daemon: {e}"))?;
        Ok(())
    })
}

fn auto_start_with(
    base: &str,
    reachable: impl FnOnce(&str) -> bool,
    launch: Option<Value>,
    spawn: impl FnOnce(&[String], &[(String, String)]) -> Result<(), String>,
) -> Result<bool, String> {
    if reachable(base) {
        return Ok(false);
    }
    let Some(launch) = launch else {
        return Ok(false);
    };
    let recorded_base = launch
        .get("base")
        .and_then(Value::as_str)
        .unwrap_or(crate::decide_ollaya::DEFAULT_BASE);
    if base != recorded_base {
        return Ok(false);
    }

    let argv: Vec<String> = launch
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if argv.is_empty() {
        return Ok(false);
    }
    let env: Vec<(String, String)> = launch
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    spawn(&argv, &env)?;
    Ok(true)
}

/// The local daemon base: the recorded one, else the documented default.
#[cfg_attr(test, mutants::skip)] // Thin config adapter; fallback selection is tested by `local_base_from`.
pub fn local_base() -> String {
    local_base_from(ollaya_launch())
}

fn local_base_from(launch: Option<Value>) -> String {
    launch
        .and_then(|l| l.get("base").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| crate::decide_ollaya::DEFAULT_BASE.to_string())
}

#[cfg_attr(test, mutants::skip)] // Environment adapter; path construction is tested by `local_root_at`.
fn local_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME")?;
    local_root_at(&PathBuf::from(home))
}

fn local_root_at(home: &std::path::Path) -> Result<PathBuf, String> {
    let root = home.join(OLLAYA_ROOT);
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    Ok(root)
}

fn run_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|e| format!("run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op raw-mode seam: test stdin is a cursor, never a terminal.
    struct FakeRaw;

    impl crate::select::RawMode for FakeRaw {
        fn enter(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn leave(&mut self) {}
    }

    type RunInvocation = (String, Vec<String>, Vec<(String, String)>);

    struct FakeLocalSetup {
        root: PathBuf,
        binary_exists: bool,
        installer_creates_binary: bool,
        unpack: UnpackEnvironment,
        /// The unpack environment probed after the fake `run` executed
        /// once, standing for the machine state after an install command.
        unpack_after: UnpackEnvironment,
        install_done: bool,
        runs: Vec<RunInvocation>,
        launch: Option<Value>,
        engine_set: bool,
    }

    impl LocalSetupRuntime for FakeLocalSetup {
        fn local_root(&mut self) -> Result<PathBuf, String> {
            Ok(self.root.clone())
        }

        fn exists(&self, path: &std::path::Path) -> bool {
            path == self.root.join("bin").join("ollaya") && self.binary_exists
        }

        fn unpack_environment(&self) -> UnpackEnvironment {
            if self.install_done {
                self.unpack_after.clone()
            } else {
                self.unpack.clone()
            }
        }

        fn run(
            &mut self,
            program: &str,
            args: &[String],
            env: &[(String, String)],
        ) -> Result<(), String> {
            self.runs
                .push((program.to_string(), args.to_vec(), env.to_vec()));
            self.install_done = true;
            if program == "sh" && self.installer_creates_binary {
                self.binary_exists = true;
            }
            Ok(())
        }

        fn record_launch(&mut self, launch: &Value) -> Result<(), String> {
            self.launch = Some(launch.clone());
            Ok(())
        }

        fn set_engine(&mut self) -> Result<(), String> {
            self.engine_set = true;
            Ok(())
        }
    }

    #[test]
    fn the_choice_parser_maps_numbers_to_menu_indices() {
        assert_eq!(parse_choice("1"), Some(0));
        assert_eq!(parse_choice(" 2\n"), Some(1));
        assert_eq!(parse_choice("3"), Some(2));
        assert_eq!(parse_choice(""), None);
        assert_eq!(parse_choice("yes"), None);
        assert_eq!(parse_choice("0"), None);
    }

    #[test]
    fn the_probe_requires_the_process_to_spawn_and_agree() {
        assert!(probe_present("true", &[]));
        assert!(!probe_present("false", &[]));
        assert!(!probe_present(
            "definitely-not-a-real-command-pixel-test",
            &[]
        ));
    }

    #[test]
    fn resolution_prefers_the_flag_then_the_stored_setting_then_reachability() {
        use crate::classify::EngineChoice;
        // The explicit flag wins over everything, no probing needed.
        assert!(matches!(
            resolve_engine(
                Some(EngineChoice::Ollaya),
                "http://127.0.0.1:9999".to_string(),
                Some("remote".to_string()),
                false
            ),
            ResolvedEngine::Local { .. }
        ));
        assert!(matches!(
            resolve_engine(Some(EngineChoice::Remote), String::new(), None, true),
            ResolvedEngine::Remote
        ));
        // Stored remote wins over a reachable local server.
        assert!(matches!(
            resolve_engine(None, String::new(), Some("remote".to_string()), true),
            ResolvedEngine::Remote
        ));
        // Stored local wins even when the server is not yet reachable (the
        // caller auto-starts it after resolution).
        assert!(matches!(
            resolve_engine(None, String::new(), Some("local".to_string()), false),
            ResolvedEngine::Local { .. }
        ));
        // auto/absent: reachability decides.
        assert!(matches!(
            resolve_engine(None, String::new(), Some("auto".to_string()), true),
            ResolvedEngine::Local { .. }
        ));
        assert!(matches!(
            resolve_engine(None, String::new(), None, false),
            ResolvedEngine::Remote
        ));
    }

    #[test]
    fn reachability_probe_rejects_malformed_and_closed_bases() {
        assert!(!server_reachable("http://127.0.0.1:1"));
        assert!(!server_reachable("not a url"));
    }

    #[test]
    fn remote_key_prompt_reads_the_provider_from_the_supplied_reader() {
        let mut input = std::io::Cursor::new(b"unknown-provider\n".to_vec());
        let mut output = Vec::new();
        let error = propose_remote_key_with(&mut input, &mut output, |_, _, _, _| {
            panic!("invalid provider must not be stored")
        })
        .unwrap_err();
        assert!(error.contains("unknown provider \"unknown-provider\""));
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Provider [openrouter]>")
        );
    }

    #[test]
    fn remote_setup_should_persist_the_selected_provider_even_without_a_key() {
        for (input, expected, key, model, base) in [
            (
                "deepseek\n test-secret \n",
                crate::decide_remote::Preset::Deepseek,
                Some("test-secret"),
                None,
                None,
            ),
            (
                "opencode-go\ngo-secret\n",
                crate::decide_remote::Preset::OpencodeGo,
                Some("go-secret"),
                None,
                None,
            ),
            (
                "\n\n",
                crate::decide_remote::Preset::Openrouter,
                None,
                None,
                None,
            ),
        ] {
            let mut stored = None;
            let mut output = Vec::new();
            propose_remote_key_with(
                &mut std::io::Cursor::new(input),
                &mut output,
                |preset, value, model, base| {
                    stored = Some((
                        preset,
                        value.map(str::to_string),
                        model,
                        base.map(str::to_string),
                    ));
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                stored,
                Some((
                    expected,
                    key.map(str::to_string),
                    model.map(str::to_string),
                    base.map(str::to_string)
                ))
            );
            assert!(!String::from_utf8(output).unwrap().contains("test-secret"));
        }
    }

    #[test]
    fn remote_setup_rejects_jev_and_local_as_chat_providers() {
        for provider in ["jev", "local"] {
            let mut input = std::io::Cursor::new(format!("{provider}\n").into_bytes());
            let mut output = Vec::new();
            let error = propose_remote_key_with(&mut input, &mut output, |_, _, _, _| {
                panic!("{provider} must not be stored as a chat preset")
            })
            .unwrap_err();
            assert!(error.contains("not a remote chat provider"), "{error}");
        }
    }

    #[test]
    fn jev_setup_routes_the_key_source_and_model_choice() {
        for (input, key, model, base) in [
            (
                "1\n1\noc-key\n",
                Some("oc-key"),
                Some("jev-1.13-free"),
                Some(OPENCODE_ZEN_BASE),
            ),
            ("2\n2\nts-key\n", Some("ts-key"), Some("jev-preview"), None),
        ] {
            let mut stored = None;
            let mut output = Vec::new();
            propose_jev_key_with(
                &mut std::io::Cursor::new(input),
                &mut output,
                |preset, value, model, base| {
                    stored = Some((
                        preset,
                        value.map(str::to_string),
                        model,
                        base.map(str::to_string),
                    ));
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                stored,
                Some((
                    crate::decide_remote::Preset::Jev,
                    key.map(str::to_string),
                    model.map(str::to_string),
                    base.map(str::to_string)
                ))
            );
            assert!(!String::from_utf8(output).unwrap().contains("oc-key"));
        }
    }

    #[test]
    fn ensure_local_policy_starts_once_and_waits_only_until_reachable() {
        let mut start_calls = 0;
        let mut reachability_checks = 0;
        let mut sleeps = 0;
        let mut reachable = || {
            reachability_checks += 1;
            reachability_checks >= 3
        };
        let mut start = || {
            start_calls += 1;
            Ok(true)
        };
        let mut sleep = |_| sleeps += 1;

        ensure_local_with(
            "http://127.0.0.1:11435",
            &mut reachable,
            &mut start,
            &mut sleep,
            4,
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(start_calls, 1);
        assert_eq!(sleeps, 1);
    }

    #[test]
    fn ensure_local_policy_does_not_poll_without_a_recorded_launch() {
        let mut reachable = || false;
        let mut start = || Ok(false);
        let mut sleeps = 0;
        let mut sleep = |_| sleeps += 1;

        ensure_local_with(
            "http://127.0.0.1:11435",
            &mut reachable,
            &mut start,
            &mut sleep,
            1,
            Duration::ZERO,
        )
        .unwrap();

        assert_eq!(sleeps, 0);
    }

    #[test]
    fn ensure_local_policy_reports_a_bounded_startup_failure() {
        let mut reachable = || false;
        let mut start = || Ok(true);
        let mut sleeps = 0;
        let mut sleep = |_| sleeps += 1;

        let error = ensure_local_with(
            "http://127.0.0.1:11435",
            &mut reachable,
            &mut start,
            &mut sleep,
            2,
            Duration::ZERO,
        )
        .unwrap_err();

        assert!(error.contains("did not come up within two minutes"));
        assert_eq!(sleeps, 2);
    }

    #[test]
    fn auto_start_should_not_spawn_a_recorded_daemon_for_another_endpoint() {
        assert!(
            !auto_start_with(
                "http://127.0.0.1:9999",
                |_| false,
                Some(json!({"base": "http://127.0.0.1:11435", "argv": ["ollaya", "serve"]})),
                |_, _| panic!("a custom URL must not start the unrelated managed daemon"),
            )
            .unwrap()
        );
        assert!(
            auto_start_with(
                "http://127.0.0.1:9999",
                |_| false,
                Some(json!({"base": "http://127.0.0.1:9999", "argv": ["ollaya", "serve"]})),
                |_, _| Ok(()),
            )
            .unwrap()
        );
    }

    #[test]
    fn auto_start_policy_only_spawns_a_valid_unreachable_launch() {
        let launch = json!({
            "argv": ["ollaya", "serve", 3],
            "env": {"OLLAYA_HOST": "127.0.0.1:11435", "ignored": false},
        });
        let mut spawned = None;
        let result = auto_start_with(
            "http://127.0.0.1:11435",
            |_| false,
            Some(launch),
            |argv, env| {
                spawned = Some((argv.to_vec(), env.to_vec()));
                Ok(())
            },
        )
        .unwrap();

        assert!(result);
        assert_eq!(
            spawned,
            Some((
                vec!["ollaya".to_string(), "serve".to_string()],
                vec![("OLLAYA_HOST".to_string(), "127.0.0.1:11435".to_string())],
            ))
        );
        assert!(
            !auto_start_with(
                "http://127.0.0.1:11435",
                |_| true,
                None,
                |_, _| { panic!("reachable daemon must not be spawned") }
            )
            .unwrap()
        );
        assert!(
            !auto_start_with(
                "http://127.0.0.1:11435",
                |_| false,
                Some(json!({"argv": []})),
                |_, _| panic!("empty argv must not be spawned"),
            )
            .unwrap()
        );
    }

    #[test]
    fn install_step_policy_honors_stored_noninteractive_and_both_choices() {
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
            Some("remote".to_string()),
            |_| panic!("stored setting must not start local setup"),
            |_, _| panic!("stored setting must not prompt for a key"),
            |_, _| panic!("stored setting must not prompt for a jev key"),
            |_, stdout| writeln!(stdout, "helpers proposed").map_err(|e| e.to_string()),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("already configured"));
        // A stored engine still gets the helpers offer.
        assert!(output.contains("helpers proposed"));

        let mut output = Vec::new();
        install_step_with(
            false,
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
            None,
            |_| panic!("non-interactive install must not start local setup"),
            |_, _| panic!("non-interactive install must not prompt for a key"),
            |_, _| panic!("non-interactive install must not prompt for a jev key"),
            |_, _| panic!("non-interactive install must not offer helpers"),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("non-interactive install")
        );

        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"1\n".to_vec()),
            &mut output,
            None,
            |stdout| writeln!(stdout, "local setup ran").map_err(|e| e.to_string()),
            |_, _| panic!("local choice must not prompt for a remote key"),
            |_, _| panic!("local choice must not prompt for a jev key"),
            |_, stdout| writeln!(stdout, "helpers proposed").map_err(|e| e.to_string()),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("local setup ran"));
        // An accepted engine leads into the helpers proposal.
        assert!(output.contains("helpers proposed"));

        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"2\nprovider input\n".to_vec()),
            &mut output,
            None,
            |_| panic!("remote choice must not start local setup"),
            |stdin, stdout| {
                let mut provider = String::new();
                stdin.read_line(&mut provider).map_err(|e| e.to_string())?;
                writeln!(stdout, "remote key for {}", provider.trim()).map_err(|e| e.to_string())
            },
            |_, _| panic!("remote choice must not prompt for a jev key"),
            |_, stdout| writeln!(stdout, "helpers proposed").map_err(|e| e.to_string()),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("remote key for provider input"));
        assert!(output.contains("helpers proposed"));

        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"3\n".to_vec()),
            &mut output,
            None,
            |_| panic!("jev choice must not start local setup"),
            |_, _| panic!("jev choice must not ask for a provider"),
            |_, stdout| writeln!(stdout, "jev key prompt ran").map_err(|e| e.to_string()),
            |_, stdout| writeln!(stdout, "helpers proposed").map_err(|e| e.to_string()),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("jev key prompt ran"));
        assert!(output.contains("helpers proposed"));

        // Skipping the engine (EOF at both pickers) must not offer helpers.
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
            None,
            |_| panic!("a skipped engine must not start local setup"),
            |_, _| panic!("a skipped engine must not prompt for a key"),
            |_, _| panic!("a skipped engine must not prompt for a jev key"),
            |_, _| panic!("a skipped engine must not offer helpers"),
            &mut FakeRaw,
            false,
        )
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("skipped"));
    }

    /// The helpers for a scratch home, with Pi's agent directory at its
    /// default place whatever `$PI_CODING_AGENT_DIR` holds.
    fn helpers_at(home: &Path) -> ClassifyHelpers {
        ClassifyHelpers::at(
            home,
            pixel_install::ClassifyPiPackage::with_agent_dir(home, None, PI_CLASSIFY_EXTENSION),
        )
    }

    #[test]
    fn classify_helpers_should_write_the_bundled_skill_and_pi_package_when_accepted() {
        let home =
            std::env::temp_dir().join(format!("pixel-classify-helpers-{}", std::process::id()));
        fs::create_dir_all(home.join(".pi/agent")).unwrap();
        fs::write(
            home.join(".pi/agent/settings.json"),
            r#"{"theme":"dark","packages":["npm:other"]}"#,
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".cursor")).unwrap();

        let mut output = Vec::new();
        propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(b"y\n".to_vec()),
            &mut output,
        )
        .unwrap();

        let skill = home.join(".claude/skills/pixel-classify/SKILL.md");
        assert_eq!(fs::read_to_string(&skill).unwrap(), CLASSIFY_SKILL);
        assert!(home.join(".codex/skills/pixel-classify/SKILL.md").is_file());
        assert!(
            home.join(".cursor/skills/pixel-classify/SKILL.md")
                .is_file()
        );
        assert!(
            home.join(".pi/agent/skills/pixel-classify/SKILL.md")
                .is_file()
        );
        // A harness root that does not exist is skipped, not created.
        assert!(!home.join(".devin").exists());
        // The tools ship as Pixel's own Pi package, declared beside the
        // user's packages; nothing lands in Pi's `extensions/`.
        let package = home.join(pixel_install::CLASSIFY_PACKAGE_DIR);
        assert_eq!(
            fs::read_to_string(package.join("extensions/pixel-classify-files.ts")).unwrap(),
            PI_CLASSIFY_EXTENSION
        );
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(package.join("package.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["pi"],
            json!({"extensions": ["./extensions/pixel-classify-files.ts"]})
        );
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(home.join(".pi/agent/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings,
            json!({"theme": "dark", "packages": ["npm:other", package.display().to_string()]})
        );
        assert!(!home.join(".pi/agent/extensions").exists());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("classify helpers: installed 7 file(s)\n")
        );
        // Everything current: a second proposal asks nothing.
        let mut again = Vec::new();
        propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(Vec::new()),
            &mut again,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(again).unwrap(),
            "classify helpers: already installed\n"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn classify_helpers_should_leave_unreadable_pi_settings_untouched() {
        let home = std::env::temp_dir().join(format!(
            "pixel-classify-helpers-bad-pi-{}",
            std::process::id()
        ));
        fs::create_dir_all(home.join(".pi/agent")).unwrap();
        fs::write(home.join(".pi/agent/settings.json"), "{ not json").unwrap();

        let err = propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(b"y\n".to_vec()),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(err.contains("is not valid JSON"), "{err}");
        assert_eq!(
            fs::read_to_string(home.join(".pi/agent/settings.json")).unwrap(),
            "{ not json"
        );
        assert!(!home.join(pixel_install::CLASSIFY_PACKAGE_DIR).exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn classify_helpers_should_write_nothing_when_declined() {
        let home = std::env::temp_dir().join(format!(
            "pixel-classify-helpers-declined-{}",
            std::process::id()
        ));

        let mut output = Vec::new();
        propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(b"n\n".to_vec()),
            &mut output,
        )
        .unwrap();

        assert!(!home.join(".claude").exists());
        assert!(String::from_utf8(output).unwrap().contains("skipped"));
    }

    #[test]
    fn classify_helpers_should_short_circuit_when_every_target_is_current() {
        let home = std::env::temp_dir().join(format!(
            "pixel-classify-helpers-idem-{}",
            std::process::id()
        ));
        let skill = home.join(".claude/skills/pixel-classify/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, CLASSIFY_SKILL).unwrap();

        let mut output = Vec::new();
        // No stdin answer: reaching the prompt would read EOF as a yes and
        // rewrite — the short-circuit must come first.
        propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
        )
        .unwrap();

        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("already installed")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn classify_helpers_should_back_up_a_differing_existing_target() {
        let home = std::env::temp_dir().join(format!(
            "pixel-classify-helpers-backup-{}",
            std::process::id()
        ));
        let skill = home.join(".claude/skills/pixel-classify/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "my edited copy").unwrap();

        propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(b"\n".to_vec()),
            &mut Vec::new(),
        )
        .unwrap();

        let backups: Vec<_> = fs::read_dir(skill.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".pixel-bak."))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read_to_string(backups[0].path()).unwrap(),
            "my edited copy"
        );
        assert_eq!(fs::read_to_string(&skill).unwrap(), CLASSIFY_SKILL);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn classify_helpers_should_stop_before_writing_when_the_backup_fails() {
        // A self-referencing symlink fails every read with ELOOP, root or
        // not, so the backup step is the first to fail and must say so.
        let home = std::env::temp_dir().join(format!(
            "pixel-classify-helpers-backup-fails-{}",
            std::process::id()
        ));
        let skill = home.join(".claude/skills/pixel-classify/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&skill, &skill).unwrap();

        let err = propose_classify_helpers_at(
            &helpers_at(&home),
            &mut std::io::Cursor::new(b"\n".to_vec()),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(
            err.starts_with(&format!("back up {}: ", skill.display())),
            "{err}"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn local_base_and_root_keep_the_documented_layout() {
        assert_eq!(
            local_base_from(Some(json!({"base": "http://localhost:9988"}))),
            "http://localhost:9988"
        );
        assert_eq!(
            local_base_from(Some(json!({"base": 12}))),
            crate::decide_ollaya::DEFAULT_BASE
        );

        let temp =
            std::env::temp_dir().join(format!("pixel-classify-setup-{}", std::process::id()));
        let root = local_root_at(&temp).unwrap();
        assert_eq!(root, temp.join(OLLAYA_ROOT));
        assert!(root.is_dir());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn reachability_probe_accepts_a_listening_tcp_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(server_reachable(&format!("http://127.0.0.1:{port}/api")));
    }

    #[test]
    fn server_reachable_within_should_answer_inside_its_cap_for_open_and_closed_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let closed = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://127.0.0.1:{}", probe.local_addr().unwrap().port())
        };
        let cap = Duration::from_millis(100);
        assert!(server_reachable_within(&open, cap));
        let started = std::time::Instant::now();
        assert!(!server_reachable_within(&closed, cap));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn local_permitted_should_refuse_only_a_stored_remote_preference() {
        assert!(!local_permitted(Some("remote")));
        for stored in [Some("local"), Some("auto"), Some("something-else"), None] {
            assert!(local_permitted(stored), "{stored:?}");
        }
    }

    #[test]
    fn process_runner_reports_a_nonzero_exit_status() {
        run_env("true", &[], &[]).unwrap();
        let error = run_env("false", &[], &[]).unwrap_err();
        assert!(error.starts_with("false failed:"));
    }

    #[test]
    fn local_setup_installs_missing_binary_pulls_model_and_records_launch() {
        let root = PathBuf::from("/pixel-test/ollaya");
        let mut runtime = FakeLocalSetup {
            root: root.clone(),
            binary_exists: false,
            installer_creates_binary: true,
            unpack: UnpackEnvironment {
                zstd_present: true,
                os: "linux",
                os_release: None,
            },
            unpack_after: UnpackEnvironment {
                zstd_present: true,
                os: "linux",
                os_release: None,
            },
            install_done: false,
            runs: Vec::new(),
            launch: None,
            engine_set: false,
        };
        let mut output = Vec::new();

        setup_local_with(&mut runtime, &mut output).unwrap();

        assert_eq!(runtime.runs.len(), 3);
        assert_eq!(runtime.runs[0].0, "curl");
        assert_eq!(runtime.runs[0].1[1], "https://ollaya.dev/install.sh");
        assert_eq!(runtime.runs[1].0, "sh");
        assert_eq!(
            runtime.runs[1].2,
            vec![
                ("OLLAYA_INSTALL_DIR".to_string(), root.display().to_string()),
                ("OLLAYA_NO_SERVICE".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(
            runtime.runs[2].0,
            root.join("bin/ollaya").display().to_string()
        );
        assert_eq!(
            runtime.runs[2].1,
            vec![
                "pull".to_string(),
                crate::decide_ollaya::DEFAULT_MODEL.to_string()
            ]
        );
        let launch = runtime.launch.unwrap();
        assert_eq!(
            launch["argv"],
            json!([root.join("bin/ollaya").display().to_string(), "serve"])
        );
        assert_eq!(
            launch["env"]["OLLAYA_MODELS"],
            json!(root.join("models").display().to_string())
        );
        assert!(runtime.engine_set);
        assert!(String::from_utf8(output).unwrap().contains("auto-start"));
    }

    #[test]
    fn local_setup_rejects_an_installer_that_does_not_create_the_binary() {
        let mut runtime = FakeLocalSetup {
            root: PathBuf::from("/pixel-test/ollaya"),
            binary_exists: false,
            installer_creates_binary: false,
            unpack: UnpackEnvironment {
                zstd_present: true,
                os: "linux",
                os_release: None,
            },
            unpack_after: UnpackEnvironment {
                zstd_present: true,
                os: "linux",
                os_release: None,
            },
            install_done: false,
            runs: Vec::new(),
            launch: None,
            engine_set: false,
        };

        let error = setup_local_with(&mut runtime, &mut Vec::new()).unwrap_err();

        assert!(error.contains("installer finished"));
        assert_eq!(runtime.runs.len(), 2);
        assert!(runtime.launch.is_none());
        assert!(!runtime.engine_set);
    }

    fn absent(env: Option<String>, os: &'static str) -> UnpackEnvironment {
        UnpackEnvironment {
            zstd_present: false,
            os,
            os_release: env,
        }
    }

    fn present(env: Option<String>, os: &'static str) -> UnpackEnvironment {
        UnpackEnvironment {
            zstd_present: true,
            os,
            os_release: env,
        }
    }

    #[test]
    fn unpack_support_names_the_distribution_package_manager() {
        let install = |env: &UnpackEnvironment| unpack_support_install(env).unwrap().join(" ");
        assert_eq!(
            install(&absent(Some("ID=ubuntu\n".into()), "linux")),
            "sudo apt-get install -y zstd"
        );
        // Raspberry Pi OS names its family through ID_LIKE, not ID.
        assert_eq!(
            install(&absent(
                Some("ID=raspios\nID_LIKE=\"debian\"\n".into()),
                "linux"
            )),
            "sudo apt-get install -y zstd"
        );
        for (release, expected) in [
            ("ID=fedora\n", "sudo dnf install -y zstd"),
            ("ID=ol\n", "sudo dnf install -y zstd"),
            (
                "ID=opensuse-leap\n",
                "sudo zypper install --non-interactive zstd",
            ),
            ("ID= endeavouros\n", "sudo pacman -S --noconfirm zstd"),
            ("ID=alpine\n", "sudo apk add zstd"),
        ] {
            let content = if release.starts_with("ID= endeavouros") {
                "ID=endeavouros\nID_LIKE=\"arch\"\n".to_string()
            } else {
                release.to_string()
            };
            assert_eq!(
                install(&absent(Some(content), "linux")),
                expected,
                "{release}"
            );
        }
        assert_eq!(install(&absent(None, "macos")), "brew install zstd");
    }

    #[test]
    fn unpack_support_refuses_what_it_cannot_name() {
        // No /etc/os-release on this Linux box.
        assert_eq!(unpack_support_install(&absent(None, "linux")), None);
        // A distribution outside the table.
        assert_eq!(
            unpack_support_install(&absent(Some("ID=plan9\n".into()), "linux")),
            None
        );
        // Only Linux ships an os-release the preflight reads.
        assert_eq!(unpack_support_install(&absent(None, "windows")), None);
    }

    #[test]
    fn os_release_field_reads_quoted_values_and_skips_absent_keys() {
        let release = "PRETTY_NAME=\"Ubuntu 24.04 LTS\"\nID=ubuntu\n\nID_LIKE=debian\n# COMMENT";
        assert_eq!(os_release_field(release, "ID"), "ubuntu");
        assert_eq!(os_release_field(release, "ID_LIKE"), "debian");
        assert_eq!(os_release_field(release, "PRETTY_NAME"), "Ubuntu 24.04 LTS");
        assert_eq!(os_release_field(release, "VERSION_ID"), "");
        assert_eq!(os_release_field(release, "COMMENT"), "");
    }

    #[test]
    fn setup_local_installs_zstd_before_the_ollaya_installer() {
        let mut runtime = FakeLocalSetup {
            root: PathBuf::from("/pixel-test/ollaya"),
            binary_exists: false,
            installer_creates_binary: true,
            unpack: absent(Some("ID=ubuntu\n".into()), "linux"),
            unpack_after: present(None, "linux"),
            install_done: false,
            runs: Vec::new(),
            launch: None,
            engine_set: false,
        };
        let mut output = Vec::new();

        setup_local_with(&mut runtime, &mut output).unwrap();

        // install, curl, sh, pull: the preflight comes first and the rest
        // of the flow still runs through its full sequence.
        assert_eq!(runtime.runs.len(), 4);
        assert_eq!(
            runtime.runs[0],
            (
                "sudo".to_string(),
                vec![
                    "apt-get".to_string(),
                    "install".to_string(),
                    "-y".to_string(),
                    "zstd".to_string(),
                ],
                vec![]
            )
        );
        assert_eq!(runtime.runs[1].0, "curl");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("sudo apt-get install -y zstd"), "{text}");
    }

    #[test]
    fn setup_local_reports_the_manual_remedy_when_the_distribution_is_unknown() {
        let mut runtime = FakeLocalSetup {
            root: PathBuf::from("/pixel-test/ollaya"),
            binary_exists: false,
            installer_creates_binary: true,
            unpack: absent(None, "linux"),
            unpack_after: absent(None, "linux"),
            install_done: false,
            runs: Vec::new(),
            launch: None,
            engine_set: false,
        };

        let error = setup_local_with(&mut runtime, &mut Vec::new()).unwrap_err();

        assert!(
            error.contains("no automatic install for os linux"),
            "{error}"
        );
        assert!(error.contains("sudo apt-get install -y zstd"), "{error}");
        assert!(runtime.runs.is_empty());
    }

    #[test]
    fn setup_local_flags_when_the_install_leaves_unpack_support_missing() {
        let mut runtime = FakeLocalSetup {
            root: PathBuf::from("/pixel-test/ollaya"),
            binary_exists: false,
            installer_creates_binary: true,
            unpack: absent(Some("ID=fedora\n".into()), "linux"),
            unpack_after: absent(Some("ID=fedora\n".into()), "linux"),
            install_done: false,
            runs: Vec::new(),
            launch: None,
            engine_set: false,
        };

        let error = setup_local_with(&mut runtime, &mut Vec::new()).unwrap_err();

        assert!(error.contains("but zstd is still missing"), "{error}");
        assert!(error.contains("sudo dnf install -y zstd"), "{error}");
        // The failing preflight stops the flow: no installer ran, no launch
        // was recorded.
        assert_eq!(runtime.runs.len(), 1);
        assert_eq!(runtime.runs[0].0, "sudo");
        assert!(runtime.launch.is_none());
    }
}
