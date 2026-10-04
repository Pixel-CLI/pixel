// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Handing an auth wall to the installed `claude-code-auth-flow`.
//!
//! A browser login is the one repair the readiness probe cannot perform
//! itself: Claude Code can be unauthenticated while every provider answers,
//! and re-probing never changes that. Under `--authenticate`, and only when
//! the Claude lane fails on an auth wall, the chain spawns `claude auth
//! login`, reads the authorize URL it prints, replays the installed flow with
//! that URL (`pixel flow replay claude-code-auth-flow --execute`), waits for
//! the login to exit, and re-probes the lane once.
//!
//! Two properties are load-bearing, and each is a pure function a test drives
//! without a process:
//!
//! - **It fires only on an auth wall.** A quota, a rate limit, a transport
//!   error or a model-not-found failure is not something a login clears, and
//!   opening the user's browser for one is an intrusion with no mechanism
//!   behind it ([`should_authenticate`]).
//! - **It never opens a browser on a guess.** A login that printed no
//!   authorize URL is refused and its child killed, rather than handing the
//!   flow an empty variable ([`flow_command`]).
//!
//! The authorize URL carries a one-time `code`/`state` payload. It reaches
//! the flow's argv and nothing else: no step, no outcome and no error in this
//! module names it.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};

use regex::Regex;
use serde::Serialize;

use super::AgentRow;
use super::agents::{Agent, FailureClass};
use super::terminal::{DEFAULT_LAUNCH_BUDGET, ScriptTerminal};

/// The flow `pixel install` writes under `~/.local/share/pixel/flows/` and
/// this chain replays. It is installed, never seeded here.
pub(crate) const CLAUDE_AUTH_FLOW: &str = "claude-code-auth-flow";

/// The flow variable that carries the login's authorize URL. Spelled here
/// once, and referenced by the action log's mask in `main.rs`, so a rename
/// cannot leave the payload unmasked in one of the two places.
pub(crate) const AUTH_URL_VAR: &str = "auth_url";

/// How long `claude auth login` gets to exit after the browser flow ran. The
/// callback the browser makes is what ends it, so this is generous on
/// purpose: a login left waiting is reported, not waited on forever.
const LOGIN_BUDGET: Duration = Duration::from_secs(120);

/// How long the driver waits between reads of the login's screen.
const LOGIN_POLL: Duration = Duration::from_millis(200);

/// What the chain ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthOutcome {
    /// `--authenticate` was given and the Claude lane needed no login.
    NotNeeded,
    /// The login finished, its child exited cleanly, the flow ran, and the
    /// re-probe found the lane ready.
    Completed,
    /// The login finished and its child exited cleanly, and the re-probe
    /// still found the lane on an auth wall.
    ///
    /// Its own outcome rather than [`AuthOutcome::Completed`] beside a false
    /// `ready_after`, because the label has to carry the whole verdict: the
    /// exit code proves `claude auth login` finished, not that Claude can
    /// reach a model, and "completed" read alone claims the sign-in the very
    /// next probe refuses.
    CompletedUnready,
    /// No authorize URL before the launch budget; nothing was opened.
    NoUrl,
    /// The login never started, or did not exit cleanly.
    LoginFailed,
    /// The browser flow itself failed.
    FlowFailed,
}

impl AuthOutcome {
    /// The words the human report prints for this outcome.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NotNeeded => "not needed",
            Self::Completed => "completed",
            Self::CompletedUnready => "the login finished but claude is still not ready",
            Self::NoUrl => "refused: the login printed no authorize URL",
            Self::LoginFailed => "the login did not finish cleanly",
            Self::FlowFailed => "the browser flow failed",
        }
    }
}

/// What the auth chain did, or why it did nothing.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthChain {
    /// The agent the chain ran for.
    pub(crate) agent: &'static str,
    /// The installed flow it replays.
    pub(crate) flow: &'static str,
    /// Each step reached, in order. The authorize URL is never one of them.
    pub(crate) steps: Vec<String>,
    pub(crate) outcome: AuthOutcome,
    /// Whether the Claude lane was ready at the end of the run. For
    /// [`AuthOutcome::NotNeeded`] this is the lane's readiness from the probe
    /// that just finished, not a second probe's.
    pub(crate) ready_after: bool,
}

impl AuthChain {
    /// The chain that had nothing to do: `--authenticate` was asked for and
    /// the lane was ready, or its failure was not an auth wall.
    pub(crate) fn not_needed(rows: &[AgentRow]) -> Self {
        Self {
            agent: Agent::Claude.name(),
            flow: CLAUDE_AUTH_FLOW,
            steps: Vec::new(),
            outcome: AuthOutcome::NotNeeded,
            ready_after: rows
                .iter()
                .any(|row| row.agent == Agent::Claude.name() && row.ready),
        }
    }

    fn ended(steps: Vec<String>, outcome: AuthOutcome, ready_after: bool) -> Self {
        Self {
            agent: Agent::Claude.name(),
            flow: CLAUDE_AUTH_FLOW,
            steps,
            outcome,
            ready_after,
        }
    }
}

/// True when `detail` opens with one of the two sentences
/// [`FailureClass::Auth`] produces. The spellings come from the classifier
/// rather than a copy of them, so a reworded reason cannot leave this trigger
/// behind.
fn is_auth_failure(detail: &str) -> bool {
    [
        FailureClass::Auth.reason(true),
        FailureClass::Auth.reason(false),
    ]
    .iter()
    .any(|wording| detail.starts_with(wording))
}

/// Whether the run should hand the Claude lane to the auth flow.
///
/// `requested` is `--authenticate`, off by default because the chain drives a
/// real browser against the user's real profile. Given it, the chain still
/// fires only for the one failure a login clears: a Claude lane that is not
/// ready on an auth wall. A quota, a rate limit, a transport error and a
/// model-not-found failure all leave the lane unready, and none of them is
/// repaired by signing in.
pub(crate) fn should_authenticate(rows: &[AgentRow], requested: bool) -> bool {
    requested
        && rows.iter().any(|row| {
            row.agent == Agent::Claude.name() && !row.ready && is_auth_failure(&row.detail)
        })
}

/// The OAuth authorize URL out of `claude auth login`'s output.
///
/// The login prints the URL once and waits for the browser callback that
/// carries the resulting code. The pattern stops at whitespace and at an
/// escape, so a coloured prompt's trailing `ESC[…m` does not ride into the
/// variable the flow opens the browser at.
pub(crate) fn extract_auth_url(output: &str) -> Option<String> {
    static AUTHORIZE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"https?://[^\s\x1b]+/oauth/authorize[^\s\x1b]*")
            .expect("the authorize pattern must compile")
    });
    AUTHORIZE
        .find(output)
        .map(|found| found.as_str().to_string())
}

/// The argv that replays the installed auth flow, run under this pixel.
///
/// `--execute` is what actually drives `agent-browser`; without it replay
/// only prints the commands. `url` is a parameter rather than read from the
/// screen here because [`flow_command`] has already refused a login that
/// printed none: this function is never reached with an empty one.
fn flow_argv(account: Option<&str>, url: &str) -> Vec<String> {
    let mut argv: Vec<String> = ["flow", "replay", CLAUDE_AUTH_FLOW, "--execute"]
        .iter()
        .map(|word| str::to_string(word))
        .collect();
    if let Some(account) = account {
        argv.push("--account".to_string());
        argv.push(account.to_string());
    }
    argv.push("--var".to_string());
    argv.push(format!("{AUTH_URL_VAR}={url}"));
    argv
}

/// The command that replays the flow for what the login printed, or the
/// refusal when it printed no authorize URL.
///
/// The refusal is the point: a login whose URL never arrived must not reach
/// the flow, which would open the user's browser at an empty location.
pub(crate) fn flow_command(
    account: Option<&str>,
    output: &str,
) -> Result<Vec<String>, AuthOutcome> {
    extract_auth_url(output)
        .map(|url| flow_argv(account, &url))
        .ok_or(AuthOutcome::NoUrl)
}

/// The outcome of a login that exited cleanly, given what the re-probe then
/// saw.
///
/// A pure function because the exit code and the readiness are two facts, and
/// a run that read only the first reported a sign-in it could not see: the
/// test drives both sides of the branch without spawning a login.
fn completed_outcome(ready: bool) -> AuthOutcome {
    if ready {
        AuthOutcome::Completed
    } else {
        AuthOutcome::CompletedUnready
    }
}

/// Drive the chain for the Claude lane and report each step it reached.
///
/// The body is process glue end to end — a pty, a subprocess that drives a
/// browser, and a child wait — so it carries no decision a test could pin.
/// Every decision it takes is a pure function above: [`should_authenticate`]
/// chose to call it, [`flow_command`] decides whether a browser is opened at
/// all, and `reprobe` supplies the lane's readiness afterwards.
#[cfg_attr(test, mutants::skip)] // spawns a real login and replays a flow that drives a browser
pub(crate) fn authenticate(account: Option<&str>, reprobe: impl Fn() -> bool) -> AuthChain {
    let mut steps = vec!["started `claude auth login`".to_string()];
    let mut argv = vec![
        "claude".to_string(),
        "auth".to_string(),
        "login".to_string(),
    ];
    if let Some(account) = account {
        argv.push("--email".to_string());
        argv.push(account.to_string());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Ok(mut terminal) = ScriptTerminal::spawn(&argv, &cwd, &[]) else {
        steps.push("`claude auth login` could not be started".to_string());
        return AuthChain::ended(steps, AuthOutcome::LoginFailed, false);
    };
    let deadline = Instant::now() + DEFAULT_LAUNCH_BUDGET;
    let flow_args = loop {
        let screen = terminal.pump().unwrap_or_default().to_string();
        match flow_command(account, &screen) {
            Ok(args) => break args,
            Err(_) if Instant::now() >= deadline => {
                terminal.stop();
                steps.push(
                    "no authorize URL before the launch budget — nothing was opened".to_string(),
                );
                return AuthChain::ended(steps, AuthOutcome::NoUrl, false);
            }
            Err(_) => std::thread::sleep(LOGIN_POLL),
        }
    };
    steps.push("read the authorize URL from the login output".to_string());
    steps.push(format!("replayed {CLAUDE_AUTH_FLOW} --execute"));
    if !run_flow(&flow_args) {
        terminal.stop();
        steps.push("the browser flow failed".to_string());
        return AuthChain::ended(steps, AuthOutcome::FlowFailed, false);
    }
    match terminal.wait_for_exit(LOGIN_BUDGET) {
        Some(status) if status.success() => {
            steps.push("`claude auth login` exited cleanly".to_string());
            let ready = reprobe();
            steps.push(format!(
                "re-probed claude: {}",
                if ready { "ready" } else { "still not ready" }
            ));
            AuthChain::ended(steps, completed_outcome(ready), ready)
        }
        _ => {
            terminal.stop();
            steps.push("`claude auth login` did not exit cleanly".to_string());
            AuthChain::ended(steps, AuthOutcome::LoginFailed, false)
        }
    }
}

/// Replay the flow under this pixel, which is what actually drives
/// `agent-browser`. Exit 0 is the flow's own verdict.
#[cfg_attr(test, mutants::skip)] // spawns `pixel flow`, which drives a browser
fn run_flow(argv: &[String]) -> bool {
    Command::new(pixel_binary())
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("ANTHROPIC_API_KEY")
        .status()
        .is_ok_and(|status| status.success())
}

/// The pixel to replay the flow through: this process where it can be found,
/// and the `pixel` on `PATH` otherwise. Never a hardcoded install path — the
/// managed binary and a side build both have to be able to run this.
#[cfg_attr(test, mutants::skip)] // reads this process's own path
fn pixel_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pixel"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The authorize URL a login prints, as the flow documents it.
    const PRINTED_URL: &str = "https://platform.claude.com/oauth/authorize?code=true&client_id=9d1c250a&response_type=code&state=8f2a";

    fn row(agent: Agent, ready: bool, detail: &str) -> AgentRow {
        AgentRow {
            agent: agent.name(),
            provider: Some("ollama"),
            ready,
            detail: detail.to_string(),
            blocker: None,
        }
    }

    #[test]
    fn the_chain_fires_only_for_a_claude_lane_stuck_on_an_auth_wall() {
        for detail in [
            "credential rejected: authentication_failed",
            "no credential configured: authentication_failed",
        ] {
            assert!(
                should_authenticate(&[row(Agent::Claude, false, detail)], true),
                "{detail} is what a login clears"
            );
        }
        assert!(
            !should_authenticate(
                &[row(Agent::Claude, false, "credential rejected: 401")],
                false
            ),
            "--authenticate is off by default: the chain drives a real browser"
        );
        for detail in [
            "quota or credits exhausted: usage limit reached",
            "rate limited: 429 too many requests",
            "probe reached no model: connection reset by peer",
            "could not start claude: No such file or directory",
        ] {
            assert!(
                !should_authenticate(&[row(Agent::Claude, false, detail)], true),
                "{detail} is not something signing in repairs"
            );
        }
        assert!(
            !should_authenticate(&[row(Agent::Claude, true, "READY")], true),
            "a ready lane needs no login"
        );
        assert!(
            !should_authenticate(
                &[row(
                    Agent::Codex,
                    false,
                    "credential rejected: unauthorized"
                )],
                true
            ),
            "the flow is about Claude's own login"
        );
    }

    #[test]
    fn the_authorize_url_is_read_from_the_login_output() {
        // The shape `claude auth login` prints: a coloured line pointing at
        // the URL, then the line that says it is waiting for the callback.
        let output = concat!(
            "\u{1b}[2mOpening the browser to sign in…\u{1b}[0m\n",
            "If it did not open, visit:\n",
            "https://platform.claude.com/oauth/authorize?code=true&client_id=9d1c250a&response_type=code&state=8f2a\n",
            "Waiting for the callback on http://localhost:54545/callback\n",
        );
        let url = extract_auth_url(output).expect("the login printed an authorize URL");
        assert_eq!(url, PRINTED_URL, "the query has to survive whole");
        assert!(
            !url.contains('\u{1b}'),
            "an escape must not ride into the variable the flow opens: {url:?}"
        );
    }

    #[test]
    fn an_escape_ends_the_url_rather_than_joining_it() {
        assert_eq!(
            extract_auth_url(&format!("{PRINTED_URL}\u{1b}[0m\n")),
            Some(PRINTED_URL.to_string()),
            "the reset after the URL is terminal decoration, not part of it"
        );
    }

    #[test]
    fn a_login_with_no_authorize_url_never_builds_a_flow_command() {
        // The refusal is the whole point: a flow handed an empty variable
        // would open the user's browser at nothing.
        for output in [
            "",
            "Waiting for authentication…\n",
            "visit https://docs.claude.com/en/docs/claude-code for help\n",
        ] {
            assert_eq!(
                flow_command(None, output),
                Err(AuthOutcome::NoUrl),
                "{output:?} must not reach the flow"
            );
        }
        assert!(
            flow_command(None, PRINTED_URL).is_ok(),
            "a login that printed the URL does build one"
        );
    }

    #[test]
    fn the_flow_command_replays_the_installed_flow_with_the_url_and_the_account() {
        let argv = flow_command(Some("someone@example.com"), PRINTED_URL).expect("a URL was read");
        assert_eq!(argv[0], "flow", "{argv:?}");
        assert_eq!(argv[1], "replay", "{argv:?}");
        assert_eq!(argv[2], CLAUDE_AUTH_FLOW, "{argv:?}");
        assert!(argv.contains(&"--execute".to_string()), "{argv:?}");
        let at = argv
            .iter()
            .position(|word| word == "--account")
            .expect("the shortcut");
        assert_eq!(argv[at + 1], "someone@example.com", "{argv:?}");
        let var = argv
            .iter()
            .position(|word| word == "--var")
            .expect("the URL var");
        assert_eq!(
            argv[var + 1],
            format!("{AUTH_URL_VAR}={PRINTED_URL}"),
            "{argv:?}"
        );

        // Without an account the shortcut is absent rather than empty:
        // `--account ""` would pick an account nobody named.
        let bare = flow_command(None, PRINTED_URL).expect("a URL was read");
        assert!(!bare.contains(&"--account".to_string()), "{bare:?}");
    }

    #[test]
    fn every_outcome_prints_its_own_sentence() {
        let labels: Vec<&str> = [
            AuthOutcome::NotNeeded,
            AuthOutcome::Completed,
            AuthOutcome::CompletedUnready,
            AuthOutcome::NoUrl,
            AuthOutcome::LoginFailed,
            AuthOutcome::FlowFailed,
        ]
        .iter()
        .map(|outcome| outcome.label())
        .collect();
        assert_eq!(
            labels,
            vec![
                "not needed",
                "completed",
                "the login finished but claude is still not ready",
                "refused: the login printed no authorize URL",
                "the login did not finish cleanly",
                "the browser flow failed",
            ]
        );
    }

    #[test]
    fn a_login_that_exited_cleanly_is_completed_only_when_the_lane_is_ready_again() {
        // The exit code says the login finished; only the re-probe says
        // whether it worked. A run that read the first alone reported
        // "completed" for a lane the very next probe still called a wall.
        assert_eq!(completed_outcome(true), AuthOutcome::Completed);
        assert_eq!(
            completed_outcome(false),
            AuthOutcome::CompletedUnready,
            "a re-probe that still sees the wall must not read as a sign-in"
        );
    }

    #[test]
    fn a_chain_with_nothing_to_do_reports_the_lane_as_it_found_it() {
        let chain =
            AuthChain::not_needed(&[row(Agent::Claude, false, "no credential configured: x")]);
        assert_eq!(chain.agent, "claude", "{chain:?}");
        assert_eq!(chain.flow, CLAUDE_AUTH_FLOW, "{chain:?}");
        assert_eq!(chain.outcome, AuthOutcome::NotNeeded, "{chain:?}");
        assert!(!chain.ready_after, "{chain:?}");
        assert!(chain.steps.is_empty(), "{chain:?}");

        let ready = AuthChain::not_needed(&[row(Agent::Claude, true, "READY")]);
        assert!(ready.ready_after, "{ready:?}");
    }
}
