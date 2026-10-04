// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel ai-cli-readify` — honest provider readiness for the four agent
//! CLIs, and the config rewrite that points them at the provider that
//! answered.
//!
//! The shape of a run is two waves, not a queue:
//!
//! 1. the provider is probed once, and it is the winner exactly when it
//!    answered;
//! 2. each selected agent is probed in its own thread against that provider,
//!    and a lane whose probe found no provider at all is reported blocked
//!    rather than launched at nothing.
//!
//! A third, optional step follows: under `--approve`, the agent's own startup
//! gate is cleared for the one workspace named on the command line. It is
//! off by default and reported either way, because a trust write outlives the
//! run and which folders get one is the user's decision, not this command's.
//!
//! A fourth, also optional, is the one step that leaves this process: under
//! `--authenticate`, a Claude lane stuck on an auth wall is handed to the
//! installed browser flow ([`auth`]), which signs the CLI in. Off by default,
//! because it drives a real browser against the user's real profile, and
//! firing only for the one failure a login clears.
//!
//! The honesty rules live in the sibling modules: [`provider`] classifies a
//! failure instead of collapsing it into "failed", [`agents`] refuses to
//! call a probe ready on an exit code alone, [`terminal`] reports a
//! startup prompt rather than guessing a keystroke at it, and [`approve`]
//! writes a trust decision only where it was asked to and says so when it
//! was not.

pub(crate) mod agents;
pub(crate) mod approve;
pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod provider;
pub(crate) mod rpc;
pub(crate) mod terminal;

use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use pixel_proto::Epistemics;
use serde::Serialize;

pub(crate) use agents::{Agent, AgentFlag};

use agents::{
    AgentProbe, CLAUDE_CREDENTIAL_ENVS, PROBE_PROMPT, any_env_set, blocked_by, devin_auth_argv,
    parse_devin_auth, parse_probe, parse_probe_with, probe_argv,
};
use approve::Approval;
use auth::{AuthChain, CLAUDE_AUTH_FLOW};
use config::{
    antigravity_settings, claude_settings, codex_config, devin_config, write_antigravity,
    write_claude, write_codex,
};
use provider::{ProbeFailure, ProbeOutcome, Provider, probe};
use terminal::{DEFAULT_LAUNCH_BUDGET, ScriptTerminal, drive_until_settled};

#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) apply: bool,
    pub(crate) timeout: Duration,
    pub(crate) agents: Vec<Agent>,
    /// Answer the startup prompts whose key the prompt itself documents.
    /// Off by default: the reference fails closed, and a guessed keystroke on
    /// a trust dialog is the user's decision, not this command's.
    pub(crate) answer_prompts: bool,
    /// Clear each agent's own startup gate — Codex's workspace and hook
    /// trust, Claude's onboarding and trust dialog. Off by default for the
    /// same reason and a stronger one: those writes survive the run.
    pub(crate) approve: bool,
    /// The folder the trust writes are about. One workspace, named, never a
    /// walk up the tree: a trust level granted to `/work` covers every folder
    /// below it, so a run that guessed would grant more than it was asked to.
    pub(crate) workspace: PathBuf,
    /// Hand a Claude lane stuck on an auth wall to the installed
    /// `claude-code-auth-flow`, which drives a real browser against the
    /// user's real profile. Off by default, and it fires only for the one
    /// failure a login clears.
    pub(crate) authenticate: bool,
    /// The account the flow should pick — the email `claude auth login` is
    /// handed and the flow's own account shortcut. Only consulted when the
    /// chain runs.
    pub(crate) account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProviderRow {
    pub(crate) provider: &'static str,
    pub(crate) ready: bool,
    /// The reply, or the classified reason there was none.
    pub(crate) detail: String,
    pub(crate) failure: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentRow {
    pub(crate) agent: &'static str,
    /// The provider that answered, when one did.
    pub(crate) provider: Option<&'static str>,
    pub(crate) ready: bool,
    pub(crate) detail: String,
    /// A startup prompt or an auth wall that stopped the probe.
    pub(crate) blocker: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) providers: Vec<ProviderRow>,
    /// The provider the agents were pointed at, if any answered.
    pub(crate) selected: Option<&'static str>,
    pub(crate) agents: Vec<AgentRow>,
    /// Config files rewritten by `--apply`.
    pub(crate) applied: Vec<String>,
    /// Configs `--apply` was asked to rewrite and refused, each line the
    /// writer's own reason and the file it names.
    ///
    /// Separate from an empty [`Report::applied`] for the reason
    /// [`Report::verified_only`] exists: a run that wrote nothing because it
    /// was not asked, and a run that wrote nothing because every write was
    /// turned away, must not read alike — the second is a failure that
    /// otherwise passes for a quiet success.
    pub(crate) refused: Vec<String>,
    /// What `--apply` would have done, when it was not given.
    pub(crate) pending: Vec<String>,
    /// Configs verified but never written, with the file named so the
    /// absence of a rewrite reads as a decision rather than a gap.
    pub(crate) verified_only: Vec<String>,
    /// What `--approve` cleared, one row per agent.
    ///
    /// `None` is "not asked" and `Some(vec![])` is "asked, nothing to do" —
    /// a distinction the report needs, because a run that never attempted an
    /// approval and a run that found nothing to approve look identical in an
    /// empty list.
    pub(crate) approvals: Option<Vec<Approval>>,
    /// What `--authenticate` did about an auth-walled Claude lane.
    ///
    /// `None` is "not asked" and `Some(NotNeeded)` is "asked, nothing to do"
    /// — the same distinction `approvals` draws, for the same reason: a run
    /// that never touched a browser and a run that found no auth wall must
    /// not read alike.
    pub(crate) auth_chain: Option<AuthChain>,
    /// That this report observed live processes rather than anything stored:
    /// the envelope `pixel classify` and `pixel web-search` disclose, in
    /// their shape.
    pub(crate) epistemics: Epistemics,
    /// The surface this run probed, named.
    pub(crate) snapshot: Snapshot,
}

/// What a run read, in the shape `pixel classify` discloses under
/// `snapshot`: the op-specific fields beside `deterministic`.
///
/// `deterministic: false` is the load-bearing field — every row above came
/// from a live probe of a provider endpoint and of the four agent CLIs, at
/// whatever state the machine was in, so two runs of the same command need
/// not agree.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Snapshot {
    /// Always `false`: nothing here is replayed from a store.
    pub(crate) deterministic: bool,
    /// The provider endpoints probed, by name.
    pub(crate) providers: Vec<&'static str>,
    /// The agent CLIs probed, by name.
    pub(crate) agents: Vec<&'static str>,
}

/// What one live probe of this machine can vouch for, and why it is no more
/// than that.
///
/// The words are the ones `classify::document` and `web_search::document`
/// emit: `closed_world` false and `lower_bound` true, because a provider or
/// an agent that answered proved it works and one that did not answer proves
/// nothing about a minute later.
pub(crate) const PROBE_BASIS: &str = "live probe of one provider endpoint and the four agent CLIs; non-deterministic, one observation of the machine's current state";

/// The confidence label the envelope carries: the verdict
/// [`Report::all_ready`] prints, spelled the same, so the JSON envelope and
/// the human line cannot disagree.
const fn confidence_label(all_ready: bool) -> &'static str {
    if all_ready { "ready" } else { "unready" }
}

/// True when `agents` is non-empty and every row in it is ready — the one
/// place that judgement is computed, so [`Report::all_ready`] and the
/// envelope's confidence label cannot drift apart.
fn every_ready(agents: &[AgentRow]) -> bool {
    !agents.is_empty() && agents.iter().all(|row| row.ready)
}

impl Report {
    /// True when every requested agent completed a real round trip.
    ///
    /// The one line of this report that is a judgement rather than an
    /// observation, and it is deliberately the strictest one available: a run
    /// where every agent but one answered is not a readiness run, and saying
    /// otherwise is exactly the narrowing the honest probe exists to refuse.
    pub(crate) fn all_ready(&self) -> bool {
        every_ready(&self.agents)
    }
}

/// Read one provider's key from the environment, if it is there and not
/// blank. A missing key is its own outcome, not a transport error: it is the
/// one failure a user can fix without touching the provider.
///
/// The process environment is the one input a test cannot arrange: emptying
/// it is a write to state every other test in this binary shares, and they
/// run in parallel. So this is the only place that reads it, and everything
/// downstream takes the key as a parameter — the branches below are asserted
/// through [`outcome_for_key`], which is handed the key instead.
#[cfg_attr(test, mutants::skip)] // reads the process environment; outcome_for_key asserts the mapping
fn key_for(provider: Provider) -> Option<String> {
    match std::env::var(provider.key_env()) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// One provider's outcome, given an already-resolved key. `key` is a
/// parameter rather than a lookup so the missing-key branch is a decision
/// this function makes, and a test can hand it `None` without touching the
/// process environment.
fn outcome_for_key<F>(
    provider: Provider,
    key: Option<String>,
    timeout: Duration,
    probe_one: &F,
) -> ProbeOutcome
where
    F: Fn(Provider, &str, Duration) -> ProbeOutcome,
{
    match key {
        None => ProbeOutcome::failed(provider, ProbeFailure::MissingKey, String::new()),
        Some(key) => probe_one(provider, &key, timeout),
    }
}

/// Probe the provider and return its row plus the winner, the provider
/// exactly when it answered.
///
/// `key` is a parameter for the same reason it is one on [`outcome_for_key`],
/// and `probe_one` is the seam a test drives with a fake: the production call
/// is [`provider::probe`], and a test substitutes a closure that answers
/// without a socket. Between them a test reaches every branch here with no
/// environment variable and no network — asserting the winner used to depend
/// on whether the machine running the suite happened to export a key.
fn probe_providers_with<F>(
    key: Option<String>,
    timeout: Duration,
    probe_one: F,
) -> (ProviderRow, Option<Provider>)
where
    F: Fn(Provider, &str, Duration) -> ProbeOutcome,
{
    let provider = Provider::Ollama;
    let outcome = outcome_for_key(provider, key, timeout, &probe_one);
    let winner = outcome.ready.then_some(provider);
    let row = ProviderRow {
        provider: provider.name(),
        ready: outcome.ready,
        detail: outcome.detail,
        failure: outcome.failure.as_ref().map(ProbeFailure::label),
    };
    (row, winner)
}

/// The production provider probe: send one real completion through whichever
/// key the environment holds.
fn probe_providers(timeout: Duration) -> (ProviderRow, Option<Provider>) {
    let provider = Provider::Ollama;
    probe_providers_with(key_for(provider), timeout, |provider, key, timeout| {
        probe(provider, provider.base_url(), key, timeout)
    })
}

/// How often the capture drains the pipes and looks at the child.
const CAPTURE_POLL: Duration = Duration::from_millis(20);

/// How many reads one drain takes before the capture looks at the child
/// again. A child that is writing continuously would otherwise hold this loop
/// — and the deadline check behind it — for as long as it keeps writing.
const CAPTURE_READS: usize = 64;

/// Run one command, capture its output, and bound it by `timeout`.
///
/// The pipes are drained here, against the same clock as the child, and read
/// non-blocking. A drain that waits for end-of-file waits for every process
/// that inherited the write end to close it, and one that never does — a
/// wrapper that backgrounds a helper, which is what several of these agents
/// do — holds the probe open past its own exit and past the timeout it was
/// given. The child is put in its own process group so the deadline can end
/// the descendants that inherited the pipes along with it, instead of leaving
/// them running against a pipe nobody reads.
#[cfg_attr(test, mutants::skip)] // a spawn-and-drain adapter; its callers are tested against a fake
fn run_capture(argv: &[String], timeout: Duration) -> io::Result<(bool, String)> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("ANTHROPIC_API_KEY")
        .process_group(0)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    set_nonblocking(&stdout)?;
    set_nonblocking(&stderr)?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let deadline = Instant::now() + timeout;
    let status = loop {
        drain(&mut stdout, &mut out);
        drain(&mut stderr, &mut err);
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                kill_group(&child);
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(CAPTURE_POLL),
        }
    };
    // Whatever the child wrote between the last drain and its exit is still
    // in the pipe: the write end is closed now, so one more read finds it.
    drain(&mut stdout, &mut out);
    drain(&mut stderr, &mut err);
    let success = status.is_some_and(|s| s.success());
    Ok((
        success,
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        ),
    ))
}

/// Take everything `reader` has buffered right now, and return as soon as it
/// has none, so the caller's next look at the child is on time. End of file
/// and any read fault both end the drain: a pipe that has been closed has
/// nothing more to give, and a pipe that failed is not going to recover
/// inside this probe.
fn drain<R: Read>(reader: &mut R, into: &mut Vec<u8>) {
    let mut buf = [0u8; 8_192];
    for _ in 0..CAPTURE_READS {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => into.extend_from_slice(&buf[..n]),
        }
    }
}

/// Put the read end of the child's output pipe in non-blocking mode, the way
/// the pty driver does: without it the first read of a silent child is the
/// one that waits for the timeout.
#[cfg_attr(test, mutants::skip)] // one syscall on a descriptor the process owns
fn set_nonblocking<R: AsRawFd>(stdout: &R) -> io::Result<()> {
    // SAFETY: `stdout` owns this process's read end of the pipe for the whole
    // call, and `F_GETFL` only reads that descriptor's status flags.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the same open descriptor, still owned by `stdout` throughout;
    // `F_SETFL` writes back the flag word read above with `O_NONBLOCK` added
    // and touches nothing else.
    let set = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if set < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// End the child and everything it started in its group, so a descendant that
/// inherited the output pipes cannot outlive the probe that timed out.
#[cfg_attr(test, mutants::skip)] // one signal to a group the child owns
fn kill_group(child: &Child) {
    // SAFETY: `child` is running, so its pid is a live group leader —
    // `process_group(0)` made it one — and `kill` only takes a pid and a
    // signal.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
}

/// Probe one agent against one provider.
///
/// The plain capture runs first because it is cheap and is what a
/// non-interactive probe wants. Only when it fails does the pty driver run,
/// because a startup prompt is the one failure the capture cannot explain:
/// it is what turns "probe reached no model" into "workspace trust
/// confirmation required".
fn probe_agent_with<F>(
    agent: Agent,
    provider: Option<Provider>,
    timeout: Duration,
    answer_prompts: bool,
    claude_credential: bool,
    run: &F,
    diagnose: &dyn Fn(Agent, bool) -> Option<&'static str>,
) -> AgentProbe
where
    F: Fn(&[String], Duration) -> io::Result<(bool, String)>,
{
    // Devin's readiness is its own auth status: it is verified, never
    // repointed at a provider, so a provider round trip would say nothing
    // about it.
    let argv = if agent == Agent::Devin {
        devin_auth_argv()
    } else {
        let Some(provider) = provider else {
            return blocked_by(
                "no provider answered",
                "the provider failed or had no key in the environment",
            );
        };
        let _ = provider;
        probe_argv(
            agent,
            PROBE_PROMPT,
            agents::DEFAULT_CLAUDE_MODEL,
            &format!("{}s", timeout.as_secs()),
            "/dev/null",
            true,
        )
    };
    match run(&argv, timeout) {
        Ok((success, output)) => {
            // Two lanes need a reader other than the reply-token one. Devin's
            // command is its auth check and carries no reply token at all,
            // and Claude's CLI answers `authentication_failed` both for a
            // credential that was rejected and for one that was never
            // configured — so its lane is handed the one fact that tells them
            // apart. The environment is read by the caller, not here: an
            // input read inside the body is an input no test can vary.
            let probe_result = match agent {
                Agent::Devin => parse_devin_auth(&output, success),
                Agent::Claude => parse_probe_with(&output, success, claude_credential),
                _ => parse_probe(&output, success),
            };
            if probe_result.ready {
                return probe_result;
            }
            // The capture failed and has no explanation: ask the pty whether
            // a startup prompt is why.
            if let Some(blocker) = diagnose(agent, answer_prompts) {
                return blocked_by(blocker, &probe_result.detail);
            }
            probe_result
        }
        Err(e) => AgentProbe::failed(format!("could not start {}: {e}", agent.executable())),
    }
}

/// Distinguishes two probes inside one process, the way `config::TEMP_SEQ`
/// does for two writers of one config file.
static EXPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The path one probe's agent writes its trajectory export to.
///
/// Unique per call rather than `<temp>/pixel-readify-<agent>.json`: that fixed
/// name was shared by every lane and every process, so two runs at once — or
/// two tests in one binary — had one probe's export land on another's, and a
/// predictable name in a world-writable directory is one anyone on the box can
/// leave there first. `seq` rather than the clock, for the reason
/// [`config::temp_for`] gives: two calls in the same tick would collide.
fn export_path(agent: Agent) -> PathBuf {
    let seq = EXPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pixel-readify-{}-{seq}-{}.json",
        std::process::id(),
        agent.name()
    ))
}

/// Drive an agent's launch under a pty and report the startup prompt that
/// stopped it, if one did.
#[cfg_attr(test, mutants::skip)] // the pty adapter; `next_step` above carries the policy
fn diagnose_startup(agent: Agent, answer_prompts: bool, timeout: Duration) -> Option<&'static str> {
    // The export is the agent's own trajectory, written where this names it
    // and read by nothing here: it is a side effect of asking the agent to
    // run, so the file is removed on the way out. A per-call name would
    // otherwise leave one trajectory per run in the temp directory.
    let export = export_path(agent);
    let argv = probe_argv(
        agent,
        PROBE_PROMPT,
        agents::DEFAULT_CLAUDE_MODEL,
        &format!("{}s", timeout.as_secs()),
        &export.to_string_lossy(),
        true,
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Interior mutability, not a shared `&mut`: the driver calls pump and
    // send one after the other, but two closures cannot hold a single
    // mutable borrow, and the terminal is only ever touched from this thread.
    let terminal = std::cell::RefCell::new(ScriptTerminal::spawn(&argv, &cwd, &[]).ok()?);
    let outcome = drive_until_settled(
        || {
            terminal
                .borrow_mut()
                .pump()
                .map(str::to_string)
                .unwrap_or_default()
        },
        |keys| {
            let _ = terminal.borrow_mut().send(keys);
        },
        answer_prompts,
        DEFAULT_LAUNCH_BUDGET.min(timeout),
        &mut std::time::Instant::now,
    );
    // A prompt still on screen after the driver ran is the most specific
    // thing that can be said, whether the driver answered it and the keys
    // did not work or it never had an answer to send. The driver's own
    // fallback ("the launch budget ran out") is the vaguer of the two, so it
    // loses to a named prompt.
    let blocker = if let Some(rule) = terminal::detect_prompt(&outcome.screen) {
        Some(rule.label)
    } else {
        outcome.blocker
    };
    let _ = std::fs::remove_file(&export);
    blocker
}

/// Probe the selected agents concurrently, each lane against the one provider
/// that answered.
///
/// `provider` is `None` when the probe found nothing to point a lane at, and
/// then no agent is launched: a lane with nowhere to go is reported blocked
/// rather than run at nothing.
fn probe_lane<F, D>(
    agents: &[Agent],
    provider: Option<Provider>,
    timeout: Duration,
    answer_prompts: bool,
    run: &F,
    diagnose: &D,
) -> Vec<AgentRow>
where
    F: Fn(&[String], Duration) -> io::Result<(bool, String)> + Sync,
    D: Fn(Agent, bool, Duration) -> Option<&'static str> + Sync,
{
    // Read once, before any lane starts: the environment is process-wide, so
    // four lanes reading it four times can only disagree with each other.
    let claude_credential = any_env_set(&CLAUDE_CREDENTIAL_ENVS);
    let rows: Mutex<Vec<AgentRow>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for agent in agents {
            let agent = *agent;
            let rows = &rows;
            scope.spawn(move || {
                // Devin's own auth is the only probe it gets, so its lane
                // carries no provider name.
                let provider_name = if agent == Agent::Devin {
                    None
                } else {
                    provider.map(Provider::name)
                };
                let probe_result = probe_agent_with(
                    agent,
                    provider,
                    timeout,
                    answer_prompts,
                    claude_credential,
                    run,
                    &|agent, answer| diagnose(agent, answer, timeout),
                );
                // A lane that answered, or that a startup prompt stopped,
                // still ran against the provider, so the row names it; one
                // that simply failed names none, and its `detail` carries the
                // reason.
                let reported = if probe_result.ready || probe_result.blocker.is_some() {
                    provider_name
                } else {
                    None
                };
                rows.lock().expect("agent rows mutex").push(AgentRow {
                    agent: agent.name(),
                    provider: reported,
                    ready: probe_result.ready,
                    detail: probe_result.detail,
                    blocker: probe_result.blocker,
                });
            });
        }
    });
    let mut rows = rows.into_inner().expect("agent rows mutex");
    rows.sort_by_key(|row| {
        Agent::ALL
            .iter()
            .position(|agent| agent.name() == row.agent)
            .unwrap_or(usize::MAX)
    });
    rows
}

/// Rewrite each requested agent's config to point at `provider`, returning
/// what was written and what was refused.
///
/// Only the agents the run asked for are touched, and only those whose config
/// this command owns: `rewrites_config` is the single place that decides the
/// second half, so adding a fifth agent cannot quietly start writing to a
/// file nobody sanctioned.
///
/// A writer's refusal is a result, not a non-event: a settings file holding
/// an `env` this command cannot merge into is left exactly as it was found,
/// and the caller has to be able to say so.
fn write_configs(
    home: &std::path::Path,
    provider: Provider,
    agents: &[Agent],
) -> (Vec<String>, Vec<String>) {
    let mut written = Vec::new();
    let mut refused = Vec::new();
    for agent in agents {
        if !agent.rewrites_config() {
            continue;
        }
        let result = match agent {
            Agent::Codex => write_codex(&codex_config(home), provider),
            Agent::Claude => write_claude(&claude_settings(home), provider),
            Agent::Antigravity => write_antigravity(&antigravity_settings(home), provider),
            // Unreachable past the `rewrites_config` guard above. Spelled out
            // so a new agent cannot arrive without a writer beside it.
            Agent::Devin => continue,
        };
        match result {
            Ok(line) => written.push(line),
            Err(reason) => refused.push(format!("{}: {reason}", agent.name())),
        }
    }
    (written, refused)
}

pub(crate) fn run(options: &Options) -> Result<Report, String> {
    let (provider_row, winner) = probe_providers(options.timeout);
    let agents = if options.agents.is_empty() {
        Agent::ALL.to_vec()
    } else {
        options.agents.clone()
    };
    let rows = probe_lane(
        &agents,
        winner,
        options.timeout,
        options.answer_prompts,
        &run_capture,
        &diagnose_startup,
    );
    let auth_chain = auth_chain_for(options, &rows, winner);
    let home = home_dir()?;
    let (applied, pending, refused) = match (winner, options.apply) {
        (Some(provider), true) => {
            let (written, refused) = write_configs(&home, provider, &agents);
            (written, Vec::new(), refused)
        }
        (Some(provider), false) => (
            Vec::new(),
            planned_configs(&home, provider, &agents),
            Vec::new(),
        ),
        (None, _) => (Vec::new(), Vec::new(), Vec::new()),
    };
    let approvals = options
        .approve
        .then(|| clear_gates(&home, &agents, &options.workspace, options.timeout));
    // The envelope is built from the rows the report carries, not from a
    // second reading of the machine: `snapshot.agents` is what was probed
    // under `--agent`, and the confidence label is the verdict
    // `all_ready()` prints.
    let snapshot = Snapshot {
        deterministic: false,
        providers: vec![provider_row.provider],
        agents: rows.iter().map(|row| row.agent).collect(),
    };
    let epistemics = Epistemics {
        closed_world: false,
        lower_bound: true,
        basis: PROBE_BASIS.to_string(),
        staleness_ms: None,
        confidence: Some(confidence_label(every_ready(&rows)).to_string()),
        extraction_limits: Vec::new(),
    };
    Ok(Report {
        providers: vec![provider_row],
        selected: winner.map(Provider::name),
        agents: rows,
        applied,
        refused,
        pending,
        verified_only: verified_only(&home, &agents),
        approvals,
        auth_chain,
        epistemics,
        snapshot,
    })
}

/// The auth chain `--authenticate` asked for, or `None` when it was not
/// given.
///
/// Glue: it either replays a real browser flow or builds the row that says
/// there was nothing to do. Both decisions are made and tested in [`auth`] —
/// [`auth::should_authenticate`] chooses the branch, and
/// [`AuthChain::not_needed`] fills the other one — so the spawn itself,
/// reached only under `--authenticate`, is the one thing with no test under
/// it.
#[cfg_attr(test, mutants::skip)] // runs a real login; the decision it branches on is tested
fn auth_chain_for(
    options: &Options,
    rows: &[AgentRow],
    provider: Option<Provider>,
) -> Option<AuthChain> {
    options.authenticate.then(|| {
        if auth::should_authenticate(rows, options.authenticate) {
            auth::authenticate(options.account.as_deref(), || {
                probe_lane(
                    &[Agent::Claude],
                    provider,
                    options.timeout,
                    options.answer_prompts,
                    &run_capture,
                    &diagnose_startup,
                )
                .first()
                .is_some_and(|row| row.ready)
            })
        } else {
            AuthChain::not_needed(rows)
        }
    })
}

/// Clear each agent's startup gate, one agent per thread.
///
/// Concurrency is safe here in a way it is not for the provider probes: each
/// agent's approval writes a file no other agent's does — Codex's
/// `config.toml` and its hook state, Claude's `~/.claude.json` — and each
/// app-server session is this process's own child. The rows come back in
/// `Agent::ALL` order regardless of which finished first, so the report does
/// not reflect a race the user cannot see.
fn clear_gates(
    home: &Path,
    agents: &[Agent],
    workspace: &Path,
    timeout: Duration,
) -> Vec<Approval> {
    let resolved: Vec<Approval> = std::thread::scope(|scope| {
        let handles: Vec<_> = agents
            .iter()
            .map(|agent| scope.spawn(move || approve::approve(home, *agent, workspace, timeout)))
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(approval) => approval,
                // A panic inside an approval is not the run's verdict on the
                // agent, and swallowing it would report a gate as cleared by
                // a thread that died before it wrote anything.
                Err(_) => Approval {
                    agent: "unknown",
                    approved: false,
                    detail:
                        "the approval thread panicked before it reported; nothing here was written"
                            .to_string(),
                },
            })
            .collect()
    });
    let mut rows = resolved;
    rows.sort_by_key(|row| {
        Agent::ALL
            .iter()
            .position(|agent| agent.name() == row.agent)
            .unwrap_or(Agent::ALL.len())
    });
    rows
}

/// What `--apply` would write, without writing it. Silence here for Devin is
/// deliberate and not an oversight, which is why it is a separate line
/// ([`Report::verified_only`]) rather than a missing one.
fn planned_configs(home: &std::path::Path, provider: Provider, agents: &[Agent]) -> Vec<String> {
    let mut planned = Vec::new();
    if agents.contains(&Agent::Codex) {
        planned.push(format!(
            "{} via {}",
            codex_config(home).display(),
            provider.name()
        ));
    }
    if agents.contains(&Agent::Claude) {
        planned.push(format!(
            "{} via {}",
            claude_settings(home).display(),
            provider.name()
        ));
    }
    if agents.contains(&Agent::Antigravity) {
        planned.push(format!(
            "{} via {}",
            antigravity_settings(home).display(),
            provider.name()
        ));
    }
    planned
}

/// The agents whose config this command verifies but never writes. Devin's
/// model is a Devin-side identifier no provider list here has an equivalent
/// for, so repointing it would mean guessing; it is reported instead.
fn verified_only(home: &std::path::Path, agents: &[Agent]) -> Vec<String> {
    agents
        .iter()
        .filter(|agent| !agent.rewrites_config())
        .map(|agent| {
            format!(
                "{}: {} verified, never rewritten",
                agent.name(),
                devin_config(home).display()
            )
        })
        .collect()
}

/// One titled block of the report, or nothing at all when it has no lines.
///
/// The three lists — what `--apply` wrote, what it would have written, and
/// what it verified without writing — are each *absent* rather than printed
/// empty, so a run that wrote nothing does not read like one that wrote
/// something. Splitting it out is what makes that rule assertable: the
/// alternative is capturing stdout, and a header nobody can test is a header
/// that comes back inverted.
fn section(title: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!("\n{title}\n");
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The human-readable report. Every line is a fact the run established: a
/// provider's classified condition, the provider each agent settled on, and
/// One provider row's text after the state column.
///
/// The label reaches the reader exactly once. A classified provider error
/// already opens with its own label in `detail` — that is what the probe
/// records, status and all — so printing the label ahead of it read as
/// `rate limited (429): rate limited (429) (HTTP 429): …`. A missing key is
/// the opposite case: there is no provider text to carry, so the label is
/// the whole message.
fn provider_line(row: &ProviderRow) -> String {
    match row.failure {
        Some(failure) if row.detail.starts_with(failure) => row.detail.clone(),
        Some(failure) if row.detail.trim().is_empty() => failure.to_string(),
        Some(failure) => format!("{failure}: {}", row.detail),
        None => row.detail.clone(),
    }
}

/// the exact file `--apply` wrote or would write. Nothing here says "ready"
/// on behalf of a probe that did not complete one.
pub(crate) fn print_report(report: &Report) {
    println!("providers");
    for row in &report.providers {
        let state = if row.ready { "ready" } else { "not ready" };
        println!("  {:<9} {state:<10} {}", row.provider, provider_line(row));
    }
    match report.selected {
        Some(provider) => println!("\nselected: {provider}"),
        None => println!("\nselected: none — every provider failed, so nothing was rewritten"),
    }
    println!("\nagents");
    for row in &report.agents {
        let state = if row.ready { "ready" } else { "not ready" };
        let via = row.provider.unwrap_or("-");
        println!(
            "  {:<12} {state:<10} via {via:<9} {}",
            row.agent, row.detail
        );
        if let Some(blocker) = row.blocker {
            println!("  {:<12} blocked: {blocker}", "");
        }
    }
    print!("{}", section("applied", &report.applied));
    print!(
        "{}",
        section("refused — left as they were", &report.refused)
    );
    print!(
        "{}",
        section("would apply (re-run with --apply)", &report.pending)
    );
    print!("{}", section("verified only", &report.verified_only));
    match &report.approvals {
        None => println!(
            "\napprovals: not attempted — re-run with --approve to clear each agent's own startup gate"
        ),
        Some(rows) => {
            println!("\napprovals");
            for row in rows {
                let state = if row.approved { "cleared" } else { "left" };
                println!("  {:<12} {state:<8} {}", row.agent, row.detail);
            }
        }
    }
    match &report.auth_chain {
        None => println!(
            "\nauthentication: not attempted — re-run with --authenticate to clear a Claude auth wall through {CLAUDE_AUTH_FLOW}"
        ),
        Some(chain) => {
            println!("\nauthentication ({})", chain.flow);
            for step in &chain.steps {
                println!("  {step}");
            }
            let state = if chain.ready_after {
                "ready"
            } else {
                "not ready"
            };
            println!("  {} — claude afterwards: {state}", chain.outcome.label());
        }
    }
    let overall = if report.all_ready() {
        "ready"
    } else {
        "not ready"
    };
    println!("\noverall: {overall}");
}

fn home_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set, so there is no agent config to read".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::ProbeOutcome;

    fn ready(provider: Provider) -> ProbeOutcome {
        ProbeOutcome::ready(provider, "READY".to_string())
    }

    fn throttled(provider: Provider) -> ProbeOutcome {
        ProbeOutcome::failed(provider, ProbeFailure::RateLimited, "429".to_string())
    }

    /// A home directory this test owns, emptied on the way in so a rerun
    /// starts clean. Named per test, so two of them cannot share one.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pixel-readify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_capture_ends_when_the_child_exits_even_if_a_descendant_holds_the_pipe() {
        // `sh -c 'sleep 5 & exit 0'` exits at once and leaves the background
        // `sleep` holding the write end it inherited. A drain that waits for
        // end of file waits for that `sleep`, which is not the probe's
        // business and is five seconds past the answer it already has.
        let started = Instant::now();
        let (success, output) = run_capture(
            &argv(&["sh", "-c", "echo ready; sleep 5 & exit 0"]),
            Duration::from_secs(5),
        )
        .expect("a shell runs");
        assert!(success, "the child exited 0: {output:?}");
        assert!(
            output.contains("ready"),
            "what the child wrote before exiting is still captured: {output:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the capture waited for a descendant that held the pipe: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_capture_that_reaches_its_deadline_kills_the_child_and_says_so() {
        // The child itself never exits. The deadline has to end it rather
        // than wait for it, and the outcome has to read as a failure: a
        // probe that reported success here would report the timeout itself
        // as the agent answering.
        let started = Instant::now();
        let (success, _) =
            run_capture(&argv(&["sh", "-c", "sleep 30"]), Duration::from_millis(300))
                .expect("a shell runs");
        assert!(!success, "a child that never exited did not succeed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline did not end the child: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_report_prints_a_section_only_when_it_has_something_in_it() {
        // The three lists are each absent rather than printed empty: a run
        // that wrote nothing must not read like one that wrote something.
        assert_eq!(
            section("applied", &[]),
            "",
            "an empty section prints no header at all"
        );
        let one = section("applied", &["a line".to_string()]);
        assert!(one.starts_with("\napplied\n"), "{one:?}");
        assert!(one.contains("\n  a line\n"), "{one:?}");
    }

    #[test]
    fn applying_writes_every_agent_that_owns_a_config_and_spares_the_verified_one() {
        let home = scratch("apply");
        let (written, refused) = write_configs(&home, Provider::Ollama, &Agent::ALL);
        assert_eq!(
            written.len(),
            3,
            "one line per rewritten config: {written:?}"
        );
        assert!(refused.is_empty(), "nothing was refused here: {refused:?}");
        for path in [
            codex_config(&home),
            claude_settings(&home),
            antigravity_settings(&home),
        ] {
            assert!(path.is_file(), "{} was not written", path.display());
        }
        assert!(
            !devin_config(&home).exists(),
            "Devin's config is verified, never rewritten"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_config_a_writer_refuses_is_reported_rather_than_counted_as_written() {
        // Claude's settings carry an `env` this writer merges into. A user
        // who has a string there is left exactly as they were — and the run
        // has to say so, because `--apply` returning a shorter `applied`
        // list with no reason is a refusal that reads as a quiet success.
        let home = scratch("apply-refused");
        let claude = claude_settings(&home);
        std::fs::create_dir_all(claude.parent().expect("a parent")).expect("scratch");
        std::fs::write(&claude, r#"{"env":"a string, not an object"}"#).expect("fixture");
        let (written, refused) = write_configs(&home, Provider::Ollama, &Agent::ALL);
        assert_eq!(written.len(), 2, "the other two were written: {written:?}");
        assert_eq!(refused.len(), 1, "one refusal, named: {refused:?}");
        assert!(refused[0].starts_with("claude"), "{refused:?}");
        assert!(
            refused[0].contains(&claude.display().to_string()),
            "the refusal names the file it left alone: {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&claude).expect("still there"),
            r#"{"env":"a string, not an object"}"#,
            "a refused config is untouched, not half-written"
        );
        let printed = section("refused — left as they were", &refused);
        assert!(
            printed.starts_with("\nrefused — left as they were\n"),
            "the report prints the refusal under its own header: {printed:?}"
        );
        assert!(
            printed.contains(&refused[0]),
            "and carries the writer's own reason: {printed:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_plan_names_the_three_configs_and_omits_the_verified_one() {
        let home = scratch("plan");
        let planned = planned_configs(&home, Provider::Ollama, &Agent::ALL);
        assert_eq!(planned.len(), 3, "{planned:?}");
        let joined = planned.join("\n");
        for path in [
            codex_config(&home),
            claude_settings(&home),
            antigravity_settings(&home),
        ] {
            assert!(
                joined.contains(&path.display().to_string()),
                "{} is missing from the plan: {joined}",
                path.display()
            );
        }
        assert!(
            planned.iter().all(|line| line.ends_with("via ollama")),
            "every line names the provider it would point at: {planned:?}"
        );
        assert!(
            !joined.contains("devin"),
            "Devin is verified rather than planned: {joined}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_approval_rows_come_back_in_the_agents_own_order() {
        // The approvals run in parallel and finish in whatever order they
        // finish. The report is the fixed order, so two runs of the same
        // state are not a race a reader has to notice.
        let home = scratch("approve-order");
        let shuffled = [
            Agent::Devin,
            Agent::Codex,
            Agent::Antigravity,
            Agent::Claude,
        ];
        let rows = clear_gates(
            &home,
            &shuffled,
            Path::new("/nonexistent-workspace-does-not-exist"),
            Duration::from_secs(1),
        );
        let order: Vec<&str> = rows.iter().map(|row| row.agent).collect();
        assert_eq!(
            order,
            vec!["codex", "claude", "antigravity", "devin"],
            "the order is Agent::ALL whatever order they finished in"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_ready_provider_is_the_winner_and_its_row_says_so() {
        let (row, winner) = probe_providers_with(
            Some("k".to_string()),
            Duration::from_secs(1),
            |provider, _key, _timeout| ready(provider),
        );
        assert_eq!(winner, Some(Provider::Ollama));
        assert_eq!(row.provider, "ollama");
        assert!(row.ready, "{row:?}");
        assert_eq!(row.detail, "READY");
        assert_eq!(row.failure, None);
    }

    #[test]
    fn a_provider_that_did_not_answer_is_not_the_winner() {
        let (row, winner) = probe_providers_with(
            Some("k".to_string()),
            Duration::from_secs(1),
            |provider, _key, _timeout| throttled(provider),
        );
        assert_eq!(winner, None);
        assert!(!row.ready, "{row:?}");
        assert_eq!(row.provider, "ollama");
        assert_eq!(row.failure, Some("rate limited (429)"));
    }

    #[test]
    fn a_provider_with_no_key_is_not_ready_and_says_which() {
        // The seam is handed `None` rather than having the environment
        // emptied, so this asserts the branch and not the machine's shell.
        let probed = Mutex::new(0);
        let probe_one = |provider: Provider, _key: &str, _timeout: Duration| {
            *probed.lock().unwrap() += 1;
            ready(provider)
        };
        let outcome = outcome_for_key(Provider::Ollama, None, Duration::from_secs(1), &probe_one);
        assert!(!outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, Some(ProbeFailure::MissingKey));
        assert_eq!(
            outcome.failure.as_ref().map(ProbeFailure::label),
            Some("no key")
        );
        assert_eq!(*probed.lock().unwrap(), 0, "no request without a key");
    }

    #[test]
    fn a_provider_with_a_key_is_probed() {
        let probe_one = |provider: Provider, _key: &str, _timeout: Duration| ready(provider);
        let outcome = outcome_for_key(
            Provider::Ollama,
            Some("k".to_string()),
            Duration::from_secs(1),
            &probe_one,
        );
        assert!(outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, None);
    }

    /// A row as the probe records one: `detail` opens with the label.
    fn failed_row(detail: &str, failure: &'static str) -> ProviderRow {
        ProviderRow {
            provider: "ollama",
            ready: false,
            detail: detail.to_string(),
            failure: Some(failure),
        }
    }

    #[test]
    fn a_classified_failure_is_named_once_in_the_report() {
        // The real line from a throttled Ollama Cloud run: the label was
        // printed ahead of a detail that already began with it.
        let row = failed_row(
            "rate limited (429) (HTTP 429): you have reached your session usage limit",
            "rate limited (429)",
        );
        let line = provider_line(&row);
        assert_eq!(
            line.matches("rate limited (429)").count(),
            1,
            "the label belongs once in the line: {line}"
        );
        assert!(
            line.contains("HTTP 429"),
            "the status has to survive: {line}"
        );
        assert!(
            line.contains("session usage limit"),
            "the provider's own words have to survive: {line}"
        );
    }

    #[test]
    fn a_missing_key_still_reads_as_its_own_reason() {
        // `MissingKey` is the one failure with no provider text to carry, so
        // the label is the whole message and must not print an empty tail.
        let row = failed_row("", "no key");
        assert_eq!(provider_line(&row), "no key");
    }

    #[test]
    fn a_blank_detail_is_treated_as_no_detail_at_all() {
        // Whitespace is not provider text: a body of spaces carries nothing
        // for the label to be joined to, so the label is the whole message.
        let row = failed_row("   ", "no key");
        assert_eq!(provider_line(&row), "no key");
    }

    #[test]
    fn a_body_that_does_not_open_with_the_label_keeps_both_halves() {
        // A gateway can answer in its own sentence instead of the one the
        // classifier keys on. The label names the condition and the body is
        // the provider's evidence; dropping either loses a fact.
        let row = failed_row(
            "the upstream model is unavailable right now",
            "server error (503)",
        );
        assert_eq!(
            provider_line(&row),
            "server error (503): the upstream model is unavailable right now"
        );
    }

    #[test]
    fn a_ready_row_prints_its_reply_alone() {
        let row = ProviderRow {
            provider: "ollama",
            ready: true,
            detail: "READY".to_string(),
            failure: None,
        };
        assert_eq!(provider_line(&row), "READY");
    }

    #[test]
    fn the_claude_lane_reaches_the_reader_with_the_fact_it_was_handed() {
        // `agents` tests the reader; this is about the wiring, which is the
        // half a reader test cannot see. Claude's CLI answers the same way
        // for a rejected credential and for one that was never configured, so
        // the two facts must produce two different reports — and until the
        // fact was a parameter, the only way to vary it was to write to the
        // process environment, which no test can do safely.
        let stream = || Ok((false, r#"{"error":"authentication_failed"}"#.to_string()));
        let probe = |claude_credential| {
            probe_agent_with(
                Agent::Claude,
                Some(Provider::Ollama),
                Duration::from_secs(1),
                false,
                claude_credential,
                &|_argv: &[String], _timeout: Duration| stream(),
                &|_, _| None,
            )
        };
        let rejected = probe(true);
        let absent = probe(false);
        assert!(
            rejected.detail.contains("credential rejected"),
            "{rejected:?}"
        );
        assert!(
            absent.detail.contains("no credential configured"),
            "{absent:?}"
        );
        assert_ne!(rejected.detail, absent.detail);
    }

    #[test]
    fn a_lane_that_answers_reports_the_provider_it_used() {
        let run =
            |_argv: &[String], _timeout: Duration| Ok((true, r#"{"text":"READY"}"#.to_string()));
        let rows = probe_lane(
            &[Agent::Codex],
            Some(Provider::Ollama),
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ready, "{rows:?}");
        assert_eq!(rows[0].provider, Some("ollama"));
    }

    #[test]
    fn a_lane_that_fails_against_the_provider_names_none() {
        let run = |_argv: &[String], _timeout: Duration| {
            Ok((true, r#"{"message":"429 too many requests"}"#.to_string()))
        };
        let rows = probe_lane(
            &[Agent::Codex],
            Some(Provider::Ollama),
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert!(!rows[0].ready, "{rows:?}");
        assert_eq!(rows[0].provider, None, "{rows:?}");
        assert!(
            !rows[0].detail.is_empty(),
            "the failure has to carry its reason: {rows:?}"
        );
    }

    #[test]
    fn a_lane_stops_at_a_blocker() {
        let run = |_argv: &[String], _timeout: Duration| Ok((false, String::new()));
        let rows = probe_lane(
            &[Agent::Claude],
            Some(Provider::Ollama),
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| Some("Workspace trust confirmation required"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].blocker,
            Some("Workspace trust confirmation required")
        );
        assert_eq!(rows[0].provider, Some("ollama"), "{rows:?}");
    }

    #[test]
    fn devin_is_probed_through_its_own_auth_and_not_a_provider() {
        let seen = Mutex::new(Vec::new());
        let run = |argv: &[String], _timeout: Duration| {
            seen.lock().unwrap().push(argv.join(" "));
            Ok((true, "Logged in as someone".to_string()))
        };
        let rows = probe_lane(
            &[Agent::Devin],
            Some(Provider::Ollama),
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows[0].provider, None, "{rows:?}");
        assert_eq!(seen.lock().unwrap().as_slice(), ["devin auth status"]);
        // The assertion this test was missing: it ran Devin's auth command,
        // saw a logged-in line, and still reported the row unready, because
        // the lane parsed its output with the reply-token reader.
        assert!(rows[0].ready, "an authenticated Devin is ready: {rows:?}");
    }

    #[test]
    fn an_agent_with_no_provider_is_blocked_rather_than_run() {
        let ran = Mutex::new(0);
        let run = |_argv: &[String], _timeout: Duration| {
            *ran.lock().unwrap() += 1;
            Ok((true, "READY".to_string()))
        };
        let rows = probe_lane(
            &[Agent::Codex],
            None,
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert!(!rows[0].ready, "{rows:?}");
        assert_eq!(rows[0].blocker, Some("no provider answered"));
        assert_eq!(rows[0].provider, None, "{rows:?}");
        assert_eq!(*ran.lock().unwrap(), 0, "nothing should have been spawned");
    }

    #[test]
    fn the_agent_order_in_the_report_is_the_fixed_one() {
        let run = |_argv: &[String], _timeout: Duration| Ok((true, "READY".to_string()));
        let rows = probe_lane(
            &[Agent::Devin, Agent::Codex],
            Some(Provider::Ollama),
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows[0].agent, "codex", "{rows:?}");
        assert_eq!(rows[1].agent, "devin", "{rows:?}");
    }

    #[test]
    fn a_report_with_no_agent_is_not_all_ready() {
        let report = Report {
            providers: Vec::new(),
            selected: None,
            agents: Vec::new(),
            applied: Vec::new(),
            refused: Vec::new(),
            pending: Vec::new(),
            verified_only: Vec::new(),
            approvals: None,
            auth_chain: None,
            epistemics: Epistemics::default(),
            snapshot: Snapshot {
                deterministic: false,
                providers: Vec::new(),
                agents: Vec::new(),
            },
        };
        assert!(!report.all_ready(), "an empty run proves nothing");
    }

    #[test]
    fn a_report_is_all_ready_only_when_every_agent_is() {
        let row = |ready: bool| AgentRow {
            agent: "codex",
            provider: Some("ollama"),
            ready,
            detail: String::new(),
            blocker: None,
        };
        let mut report = Report {
            providers: Vec::new(),
            selected: Some("ollama"),
            agents: vec![row(true)],
            applied: Vec::new(),
            refused: Vec::new(),
            pending: Vec::new(),
            verified_only: Vec::new(),
            approvals: None,
            auth_chain: None,
            epistemics: Epistemics::default(),
            snapshot: Snapshot {
                deterministic: false,
                providers: Vec::new(),
                agents: Vec::new(),
            },
        };
        assert!(report.all_ready());
        report.agents.push(row(false));
        assert!(!report.all_ready(), "one unready agent is not ready");
    }

    #[test]
    fn two_probes_never_share_one_export_path() {
        // The name used to be `<temp>/pixel-readify-<agent>.json`: one file
        // for every lane of every run on the machine, so two runs at once —
        // or two tests in one binary — had one probe's trajectory overwritten
        // by another's, and it was left behind when the probe ended.
        let first = export_path(Agent::Devin);
        let second = export_path(Agent::Devin);
        assert_ne!(first, second, "two probes must not share one export file");
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("pixel-readify-{}-", std::process::id())),
            "the name is qualified by the process that made it: {name}"
        );
        assert!(
            name.ends_with("devin.json"),
            "the agent it belongs to is still in the name: {name}"
        );
    }

    #[test]
    fn the_envelope_confidence_is_the_verdict_the_report_prints() {
        // The label is the one claim the envelope makes about the answer as a
        // whole, so it has to be the same word `overall:` prints — not a
        // second opinion that can drift from it.
        assert_eq!(confidence_label(true), "ready");
        assert_eq!(confidence_label(false), "unready");
    }

    #[test]
    fn the_provider_reads_its_own_key_variable() {
        // The report names the variable a user has to set, so it has to be
        // the provider's own name.
        assert_eq!(Provider::Ollama.key_env(), "OLLAMA_API_KEY");
    }
}
