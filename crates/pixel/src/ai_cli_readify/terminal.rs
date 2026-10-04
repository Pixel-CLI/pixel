// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Startup-prompt detection and dismissal.
//!
//! The TypeScript stack this port mirrors detects these prompts and *fails
//! closed*: it records a blocker and tears the child down, and the only
//! bytes it ever writes to a terminal are `\x03`. There is no dismissal
//! logic in it to port, so this module is new work and says so.
//!
//! That shapes the design. Detection is cheap and the patterns below are the
//! reference's own, verbatim. *Answering* is only honest where the prompt
//! itself documents the key — `press enter to continue` is a dismissal
//! anyone can verify by reading it. Everywhere else a byte sequence would be
//! a guess, and a guessed Enter on a trust dialog is a security decision
//! taken on the user's behalf, so those rules carry [`Answer::Unknown`]: the
//! driver names the prompt and stops, exactly as the reference does, and a
//! human pins the sequence once it has been observed.

use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};

use regex::Regex;

/// How often the driver looks at the screen while waiting for a prompt.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the driver waits for the child to reach a stable screen. The
/// reference's own cap for a fresh launch is 30 s.
pub(crate) const DEFAULT_LAUNCH_BUDGET: Duration = Duration::from_secs(30);

/// A byte a rule may send. Only the ones a prompt documents are listed.
pub(crate) const ENTER: &[u8] = b"\r";
/// The interrupt the reference sends on teardown.
pub(crate) const CTRL_C: &[u8] = b"\x03";

/// How much of the child's output is kept for matching. An agent that streams
/// for minutes must not grow the buffer without limit while the only thing
/// ever matched is the last screenful.
const SCREEN_CAP_CHARS: usize = 65_536;

/// How many `read`s one [`ScriptTerminal::pump`] makes before it hands the
/// driver back its clock.
///
/// A child that streams without pausing keeps the pty readable for as long as
/// it runs, so a drain that follows the output rather than a count never
/// returns and leaves the launch budget with nothing to measure. Stopping
/// mid-screen costs nothing: the next pump continues from where this one
/// stopped.
const PUMP_READS: usize = 16;

/// What to send when a prompt appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Bytes the prompt's own text documents as the way through.
    Keys(&'static [u8]),
    /// No verified sequence exists. The driver reports the prompt and stops.
    Unknown,
}

/// One recognised startup prompt.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptRule {
    /// The reference's pattern, verbatim.
    pub(crate) pattern: &'static str,
    /// What the prompt means, in the report's own words.
    pub(crate) label: &'static str,
    pub(crate) answer: Answer,
}

/// Every startup prompt the reference recognises, with its pattern and its
/// emitted label kept exactly as that stack words them, so a report from
/// here reads the same as a report from there.
///
/// The reference's fourth rule carries four alternatives in one pattern —
/// `update available`, `restart … to update`, `press … enter … continue` and
/// `select|choose … theme|appearance`. It is split in two here, and the split
/// is the only departure from that table. The reference has no answer for any
/// of them, so the split costs nothing there; here, where a prompt that
/// documents its own key may be answered, keeping them together would mean
/// sending Enter at a theme picker on the strength of a sentence that is not
/// on screen. The narrower pattern is listed first because detection is
/// first-match-wins.
pub(crate) const PROMPT_RULES: [PromptRule; 6] = [
    PromptRule {
        pattern: r"(?i)trust (?:this|the) (?:folder|workspace|directory)|do you trust|workspace.{0,30}(untrusted|not trusted)|quick safety check",
        label: "Workspace trust confirmation required",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)(?:review|trust).{0,30}hooks|hooks.{0,40}(?:need|require).{0,20}(?:review|trust)|untrusted hooks",
        label: "Hook review/trust required — use /hooks",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)sign in to|log in to|login required|authentication.{0,20}(expired|failed)|choose.*login|select.*login",
        label: "Sign-in required — the CLI is waiting for a login",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)press.{0,15}enter.{0,30}(continue|restart)",
        label: "Update, restart, or startup confirmation required",
        // The prompt states its own key ("press enter to continue"), which is
        // the one dismissal a reader can verify without observing a run.
        answer: Answer::Keys(ENTER),
    },
    PromptRule {
        pattern: r"(?i)update available|new version available|restart.{0,20}(required|to update)|select.{0,15}(theme|appearance)|choose.{0,15}(theme|appearance)",
        label: "Update, restart, or startup confirmation required",
        // A theme picker names no key, so there is nothing here a reader
        // could verify; it is reported like every other prompt with no
        // documented answer.
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)failed to (load|start|connect)|invalid configuration|configuration error|MCP.{0,40}(failed|error)|quota.{0,20}exhaust|rate.?limited",
        label: "Startup error or unavailable service",
        answer: Answer::Unknown,
    },
];

/// The rules as compiled patterns, in declaration order. Compiling once
/// keeps a 200 ms poll loop from recompiling five regexes a second.
static COMPILED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PROMPT_RULES
        .iter()
        .map(|rule| Regex::new(rule.pattern).expect("a PROMPT_RULES pattern must compile"))
        .collect()
});

/// The first prompt in `screen`, or `None` for a clean screen.
///
/// Order is the table's order, so a screen carrying two prompts reports the
/// earlier rule; the table lists trust and hooks before the generic
/// service-error rule for that reason.
pub(crate) fn detect_prompt(screen: &str) -> Option<&'static PromptRule> {
    PROMPT_RULES
        .iter()
        .zip(COMPILED.iter())
        .find(|(_, pattern)| pattern.is_match(screen))
        .map(|(rule, _)| rule)
}

/// One step of the driver's decision, kept pure so the whole policy is
/// testable without a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The screen is clean; keep polling.
    Wait,
    /// Send these bytes and keep polling.
    Send(&'static [u8]),
    /// A prompt no rule can answer. Report it and stop.
    Blocked(&'static str),
}

/// What to do about the current screen. A departure from the reference lives
/// here and nowhere else: answering is opt-in, so the default run reports
/// the prompt and stops exactly as the reference does.
pub(crate) fn next_step(screen: &str, answer_prompts: bool) -> Step {
    match detect_prompt(screen) {
        None => Step::Wait,
        Some(rule) => match (answer_prompts, rule.answer) {
            (true, Answer::Keys(keys)) => Step::Send(keys),
            _ => Step::Blocked(rule.label),
        },
    }
}

/// A child attached to a pseudo-terminal.
///
/// Implemented over `script(1)`, which allocates the pty and forwards both
/// directions, so no unsafe `openpty` and no new dependency is needed. Its
/// flags differ between the BSD and util-linux versions and the split below
/// is the one this repository already uses for its own terminal tests.
pub(crate) struct ScriptTerminal {
    child: Child,
    stdout: ChildStdout,
    screen: String,
    read_buf: [u8; 8_192],
}

impl ScriptTerminal {
    /// Spawn `argv` under a pty in `cwd`. The caller owns the child's
    /// lifetime; [`ScriptTerminal::stop`] ends it.
    //
    // The `script` invocation is the one thing here whose branches are
    // chosen by `cfg!(target_os)`: only one of the two is compiled into a
    // given test run, so a mutant in the other survives however many real
    // children the tests spawn, and would be reported as a survivor nothing
    // can kill. The rest is not unverified — the child returned here is what
    // every driver test in this module reads, and `shell_quote`, which the
    // util-linux branch is built on, carries its own test.
    #[cfg_attr(test, mutants::skip)] // platform-exclusive argv branches; only one is compiled per test run
    pub(crate) fn spawn(
        argv: &[String],
        cwd: &Path,
        environment: &[(String, String)],
    ) -> io::Result<Self> {
        let mut command = Command::new("script");
        if cfg!(target_os = "macos") {
            command.args(["-q", "/dev/null"]);
        } else {
            command.args(["-qec"]);
        }
        if cfg!(target_os = "macos") {
            command.args(argv);
        } else {
            let line = argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            command.arg(line).arg("/dev/null");
        }
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("ANTHROPIC_API_KEY");
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        if let Err(e) = set_nonblocking_reads(&stdout) {
            // The child is running and no caller owns it yet: it must not
            // outlive a terminal that was never handed over.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(Self {
            child,
            stdout,
            screen: String::new(),
            read_buf: [0u8; 8_192],
        })
    }

    /// Send raw bytes to the child's terminal.
    pub(crate) fn send(&mut self, keys: &[u8]) -> io::Result<()> {
        let stdin =
            self.child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "the child has no stdin")
            })?;
        stdin.write_all(keys)?;
        stdin.flush()
    }

    /// Everything the child has written since the last call, appended to the
    /// running screen.
    ///
    /// The read end is non-blocking and one call drains at most [`PUMP_READS`]
    /// chunks, so the call always returns: `script` holds the pty open for the
    /// child's whole life, and a driver waiting in `read(2)` waits for a child
    /// that may have nothing to say and no reason to stop, with its budget
    /// left with nothing to measure. A `WouldBlock` read is the end of this
    /// screen rather than a failure.
    pub(crate) fn pump(&mut self) -> io::Result<&str> {
        drain_screen(&mut self.stdout, &mut self.screen, &mut self.read_buf)?;
        Ok(&self.screen)
    }

    /// Wait for the child to exit within `budget`, or give up on it.
    ///
    /// `None` means the budget ran out with the child still running: the
    /// caller owns the teardown, and a login left waiting is reported rather
    /// than waited on.
    //
    // The one caller is `auth::authenticate`, which runs a real
    // `claude auth login` and a real browser flow; no test reaches this
    // without both. The budget policy around a launch that does settle is
    // tested where it lives — `drive_until_settled` is not skipped, and its
    // own deadline (`>= deadline`) carries the mutants this would duplicate.
    #[cfg_attr(test, mutants::skip)] // reached only under `--authenticate`, which runs a real login
    pub(crate) fn wait_for_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() >= deadline => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return None,
            }
        }
    }

    /// End the child the way the reference does: interrupt, then escalate.
    //
    // `mutants::skip` because the loop's boundary carries an equivalent
    // mutant — `<` for `<=` differs only at the instant `now == deadline`,
    // which no test can arrange and after which both sides kill anyway — and
    // the attribute is function-granular, so exempting that one exempts its
    // siblings too. The grace period itself is not unverified: a teardown
    // that escalated early is what `the_teardown_lets_the_child_act_on_the
    // _interrupt_before_the_kill` catches, and it fails with `signal Some(9)`
    // under a shortened or skipped grace. Same shape, same attribute as
    // `Drop for Session` in rpc.rs.
    #[cfg_attr(test, mutants::skip)] // the boundary instant is unarrangeable; the grace period is covered by the teardown test
    pub(crate) fn stop(&mut self) {
        let _ = self.send(CTRL_C);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ScriptTerminal {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One [`ScriptTerminal::pump`], over any reader.
///
/// Split out so the drain's own policy — the cap, and the three error kinds —
/// is reachable from a test with a scripted reader: a real pty can be asked
/// for a `WouldBlock` by a child with nothing to say, but never for an
/// `Interrupted` or any other error on demand. The production path is
/// unchanged: `pump` hands it the child's read end, its screen and its read
/// buffer.
/// Keep the last [`SCREEN_CAP_CHARS`] characters of `screen`, cutting on a
/// character boundary.
///
/// The cap is a character count, so the cut has to be one.
/// `screen.len() - SCREEN_CAP_CHARS` is a byte count: it lands inside a
/// character as soon as the screen ends in a three-byte one, and `drain`
/// panics off a boundary instead of trimming. Counting the characters to
/// drop and taking the byte offset of the first one to keep gives a
/// boundary by construction.
///
/// Written without a comparison on purpose: `if count > CAP { trim }`
/// leaves the case `count == CAP` doing nothing under either operator, so
/// the `>=` mutant is equivalent and no test can hold it. `drop` is zero
/// when the screen is inside the cap, and a zero drop cuts at offset zero,
/// which is no cut at all.
fn trim_to_cap(screen: &mut String) {
    let drop = screen.chars().count().saturating_sub(SCREEN_CAP_CHARS);
    let at = screen.char_indices().nth(drop).map_or(0, |(at, _)| at);
    screen.drain(..at);
}

fn drain_screen<R: Read>(reader: &mut R, screen: &mut String, buf: &mut [u8]) -> io::Result<()> {
    for _ in 0..PUMP_READS {
        match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                screen.push_str(&chunk);
                trim_to_cap(screen);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Put the read end of the child's output pipe in non-blocking mode, so a
/// read returns the moment the child has nothing buffered rather than waiting
/// for its next word.
///
/// The mode belongs to the descriptor, so it is a one-time property of the
/// terminal rather than something every read asks for; [`ScriptTerminal::pump`]
/// takes the resulting `WouldBlock` as the end of the current screen.
#[cfg_attr(test, mutants::skip)] // one syscall on a descriptor the process owns; the prompt-return contract is tested against real children
fn set_nonblocking_reads(stdout: &ChildStdout) -> io::Result<()> {
    // SAFETY: `stdout` owns this process's read end of the pty pipe for the
    // whole call, and `F_GETFL` only reads that descriptor's status flags.
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

/// What a driven launch ended as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchOutcome {
    /// The screen reached a stable clean state, or a documented prompt was
    /// answered and it then did.
    pub(crate) clean: bool,
    /// Prompts that appeared, in the order they were seen.
    pub(crate) prompts: Vec<&'static str>,
    /// The prompt that stopped the launch, if one did.
    pub(crate) blocker: Option<&'static str>,
    /// The last screen, for the report.
    pub(crate) screen: String,
}

/// Drive a launch until its screen is stable and clean, a prompt stops it,
/// or the budget runs out.
///
/// `pump` and `send` are the two ends of the terminal, kept as closures so
/// the whole policy runs against a script in a test. `now` is a parameter
/// for the same reason: a test drives the loop's clock instead of waiting on
/// one, and the budget keeps a broken loop failing in seconds rather than
/// hanging.
pub(crate) fn drive_until_settled<P, S>(
    mut pump: P,
    mut send: S,
    answer_prompts: bool,
    budget: Duration,
    now: &mut dyn FnMut() -> Instant,
) -> LaunchOutcome
where
    P: FnMut() -> String,
    S: FnMut(&'static [u8]),
{
    let started = now();
    let mut prompts: Vec<&'static str> = Vec::new();
    let mut answered: Vec<&'static str> = Vec::new();
    // The last prompt-free screen, so a clean fragment is settled only once a
    // second pump has seen it unchanged.
    let mut settled: Option<String> = None;
    loop {
        let screen = pump();
        match next_step(&screen, answer_prompts) {
            Step::Blocked(label) => {
                return LaunchOutcome {
                    clean: false,
                    prompts,
                    blocker: Some(label),
                    screen,
                };
            }
            Step::Send(keys) => {
                // Record the rule once: a screen that keeps showing the same
                // prompt must not append it forever.
                if let Some(rule) = detect_prompt(&screen)
                    && !answered.contains(&rule.label)
                {
                    answered.push(rule.label);
                    prompts.push(rule.label);
                }
                send(keys);
            }
            Step::Wait => {
                // A clean screen settles only once a second pump has seen it
                // unchanged. Startup output arrives in stages, and a clean
                // fragment is not proof that nothing follows it: a trust or
                // hook prompt one pump later would be lost by a driver that
                // returned on the first fragment.
                if !screen.trim().is_empty() {
                    if settled.as_deref() == Some(screen.as_str()) {
                        return LaunchOutcome {
                            clean: true,
                            prompts,
                            blocker: None,
                            screen,
                        };
                    }
                    settled = Some(screen.clone());
                }
            }
        }
        if now().duration_since(started) >= budget {
            return LaunchOutcome {
                clean: false,
                prompts,
                blocker: Some("no verified interface before the launch budget ran out"),
                screen,
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Quote one argv element for the util-linux `script -c` line.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// A clock a test advances by hand, so a budget can be crossed without
    /// waiting for it.
    struct FakeClock {
        elapsed: Duration,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                elapsed: Duration::ZERO,
            }
        }
    }

    #[test]
    fn a_clean_screen_waits() {
        assert_eq!(next_step("$ ", false), Step::Wait);
        assert_eq!(next_step("", false), Step::Wait);
    }

    #[test]
    fn the_trust_prompt_is_recognised() {
        let step = next_step("Do you trust the files in this folder?", false);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn the_hook_prompt_is_recognised() {
        let step = next_step("Hooks need review before they can run", false);
        assert_eq!(
            step,
            Step::Blocked("Hook review/trust required — use /hooks")
        );
    }

    #[test]
    fn the_auth_prompt_is_recognised() {
        let step = next_step("Please sign in to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Sign-in required — the CLI is waiting for a login")
        );
    }

    #[test]
    fn an_expired_authentication_is_recognised() {
        let step = next_step("authentication expired, please sign in to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Sign-in required — the CLI is waiting for a login")
        );
    }

    #[test]
    fn a_startup_service_error_is_recognised() {
        let step = next_step("failed to connect to the model service", false);
        assert_eq!(step, Step::Blocked("Startup error or unavailable service"));
    }

    #[test]
    fn a_rate_limit_on_the_startup_screen_is_recognised() {
        let step = next_step("rate limited, try again later", false);
        assert_eq!(step, Step::Blocked("Startup error or unavailable service"));
    }

    #[test]
    fn nothing_is_sent_by_default_even_for_a_documented_prompt() {
        // The reference fails closed, and the default run matches it: a
        // guessed keystroke on a trust dialog is a decision not ours to take.
        let step = next_step("press enter to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Update, restart, or startup confirmation required")
        );
    }

    #[test]
    fn a_documented_prompt_is_answered_when_answering_is_opted_into() {
        let step = next_step("press enter to continue", true);
        assert_eq!(step, Step::Send(ENTER));
    }

    #[test]
    fn a_theme_picker_is_recognised_though_it_names_no_key() {
        // The reference folds this into the rule above. Kept apart, because
        // the answer that rule carries — Enter — is justified by a sentence
        // this screen does not contain.
        for screen in [
            "Select a theme",
            "choose appearance",
            "restart required to update",
        ] {
            let step = next_step(screen, false);
            assert_eq!(
                step,
                Step::Blocked("Update, restart, or startup confirmation required"),
                "{screen}"
            );
        }
    }

    #[test]
    fn a_theme_picker_is_not_answered_even_when_answering_is_opted_into() {
        for screen in ["Select a theme", "choose appearance to continue"] {
            let step = next_step(screen, true);
            assert_eq!(
                step,
                Step::Blocked("Update, restart, or startup confirmation required"),
                "nothing about {screen} documents a key to send"
            );
        }
    }

    #[test]
    fn a_screen_offering_both_alternatives_answers_the_one_that_documents_its_key() {
        // First-match-wins, and the narrower pattern is listed first: a
        // screen that says both must reach the answerable rule rather than
        // the generic one beside it.
        let step = next_step("Update available — press enter to continue", true);
        assert_eq!(step, Step::Send(ENTER));
    }

    #[test]
    fn an_undocumented_prompt_stays_blocked_even_when_answering_is_opted_into() {
        let step = next_step("Do you trust the files in this folder?", true);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn an_untrusted_workspace_is_recognised_in_its_other_wording() {
        let step = next_step("This workspace is untrusted", false);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn an_untrusted_hooks_warning_is_recognised() {
        let step = next_step("untrusted hooks are configured", false);
        assert_eq!(
            step,
            Step::Blocked("Hook review/trust required — use /hooks")
        );
    }

    #[test]
    fn the_driver_settles_on_a_clean_screen() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || "$ ready\n".to_string(),
            |_| {},
            false,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert!(outcome.clean, "{outcome:?}");
        assert_eq!(outcome.blocker, None);
        assert!(outcome.prompts.is_empty());
    }

    #[test]
    fn a_clean_fragment_is_not_settled_before_a_later_prompt_appears() {
        // Startup output arrives in stages. A driver that settles on the
        // first prompt-free pump returns before the trust prompt — one pump
        // later — is ever read, and the launch is reported clean although a
        // blocker is on screen.
        let script = ["$ welcome\n", "$ welcome\nDo you trust this folder?"];
        let mut pump = 0_usize;
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || {
                let screen = script[pump.min(script.len() - 1)];
                pump += 1;
                screen.to_string()
            },
            |_| {},
            false,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert_eq!(
            outcome.blocker,
            Some("Workspace trust confirmation required"),
            "the prompt on the second pump must be the reported blocker"
        );
    }

    #[test]
    fn a_screen_that_never_settles_twice_ends_the_drive_at_its_budget() {
        // The stricter settle rule must not turn a live screen into a hang: a
        // screen that changes on every pump runs out the budget like any
        // other launch that never reaches a stable screen.
        let mut pump = 0_usize;
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || {
                pump += 1;
                format!("line {pump}\n")
            },
            |_| {},
            false,
            Duration::from_millis(500),
            &mut || {
                clock.elapsed += Duration::from_millis(200);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert_eq!(
            outcome.blocker,
            Some("no verified interface before the launch budget ran out")
        );
    }

    #[test]
    fn the_driver_stops_at_a_prompt_it_cannot_answer() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || "Do you trust this folder?".to_string(),
            |_| panic!("a prompt with no verified answer must not be typed at"),
            false,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert_eq!(
            outcome.blocker,
            Some("Workspace trust confirmation required")
        );
    }

    #[test]
    fn the_driver_ends_on_budget_when_the_screen_stays_blank() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            String::new,
            |_| {},
            false,
            Duration::from_millis(500),
            &mut || {
                clock.elapsed += Duration::from_millis(200);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert!(outcome.blocker.is_some(), "{outcome:?}");
    }

    #[test]
    fn the_driver_sends_the_documented_key_and_records_the_prompt_once() {
        let mut clock = FakeClock::new();
        let sent: std::cell::RefCell<Vec<Vec<u8>>> = std::cell::RefCell::new(Vec::new());
        let outcome = drive_until_settled(
            || "press enter to continue".to_string(),
            |keys| sent.borrow_mut().push(keys.to_vec()),
            true,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert_eq!(
            outcome.prompts,
            vec!["Update, restart, or startup confirmation required"]
        );
        assert_eq!(outcome.prompts.len(), 1, "the prompt must not repeat");
        // The bytes must actually leave: the driver's whole purpose is to
        // get past this prompt, and a recorded-but-unsent key would report a
        // prompt answered that the terminal never saw.
        let sent = sent.into_inner();
        assert!(!sent.is_empty(), "nothing was typed at the prompt");
        assert!(sent.iter().all(|keys| keys == ENTER), "{sent:?}");
    }

    #[test]
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    /// A reader whose every call a test decides, so the drain's cap and its
    /// three error arms are reachable without a child. A real pty can be
    /// asked for the one error a silent child produces (`WouldBlock`), but an
    /// `Interrupted` or a plain fault cannot be arranged on demand through a
    /// kernel interface, and the cap needs a child that writes 64 KiB of
    /// bytes the test chooses.
    struct ScriptedReader<F>(F)
    where
        F: FnMut(&mut [u8]) -> io::Result<usize>;

    impl<F> Read for ScriptedReader<F>
    where
        F: FnMut(&mut [u8]) -> io::Result<usize>,
    {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (self.0)(buf)
        }
    }

    /// One drain, over a scripted reader, with the screen it produced.
    fn drain<R: Read>(reader: &mut R) -> io::Result<String> {
        let mut screen = String::new();
        let mut buf = [0u8; 8_192];
        drain_screen(reader, &mut screen, &mut buf)?;
        Ok(screen)
    }

    #[test]
    fn a_would_block_read_ends_the_screen_rather_than_failing() {
        // A child with nothing to say leaves the pty readable and empty. The
        // read that finds it so is the end of this screen, not a failed
        // launch: reading it as a fault would fail every silent child.
        let mut reader = ScriptedReader(|_: &mut [u8]| Err(io::ErrorKind::WouldBlock.into()));
        let screen = drain(&mut reader).expect("a would-block read ends the screen");
        assert_eq!(screen, "");
    }

    #[test]
    fn an_interrupted_read_is_retried_rather_than_reported() {
        // A signal landing during the read interrupts it without data and
        // without fault; the byte after it must still arrive, and the empty
        // read that follows it must end the screen.
        let mut calls = 0_u32;
        let mut reader = ScriptedReader(move |buf: &mut [u8]| {
            calls += 1;
            match calls {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => {
                    buf[..2].copy_from_slice(b"ok");
                    Ok(2)
                }
                _ => Ok(0),
            }
        });
        let screen = drain(&mut reader).expect("an interrupted read is a retry, not a failure");
        assert_eq!(screen, "ok");
    }

    #[test]
    fn a_read_error_that_is_neither_would_block_nor_interrupted_is_reported() {
        // Anything else is a real fault, and the driver has to hear about it:
        // swallowing it would report a broken terminal as a clean screen.
        let mut reader = ScriptedReader(|_: &mut [u8]| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the pty is gone",
            ))
        });
        let error = drain(&mut reader).expect_err("a real read fault must be reported");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_screen_past_the_cap_is_held_at_the_cap() {
        // A full read buffer on every one of `PUMP_READS` reads is twice the
        // cap: the screen has to come back down to it rather than keep
        // everything a stream has ever written.
        let mut reader = ScriptedReader(|buf: &mut [u8]| {
            buf.fill(b'x');
            Ok(buf.len())
        });
        let screen = drain(&mut reader).expect("a full read buffer is not an error");
        assert_eq!(screen.chars().count(), SCREEN_CAP_CHARS);
        assert_eq!(screen.len(), SCREEN_CAP_CHARS);
    }

    #[test]
    fn a_screen_of_wide_characters_at_the_cap_is_not_cut_in_half() {
        // The cap counts characters, so a screen that reaches it in two-byte
        // characters is left as it is. The boundary is exact here: every one
        // of `PUMP_READS` reads fills the buffer with `é`, so the drain ends
        // on 65536 characters and 131072 bytes, and a cap measured in bytes
        // would cut the screen in half at that point.
        let mut reader = ScriptedReader(|buf: &mut [u8]| {
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = if i % 2 == 0 { 0xc3 } else { 0xa9 };
            }
            Ok(buf.len())
        });
        let screen = drain(&mut reader).expect("a full read buffer is not an error");
        assert_eq!(screen.chars().count(), SCREEN_CAP_CHARS);
        assert_eq!(screen.len(), 2 * SCREEN_CAP_CHARS);
    }

    #[test]
    fn a_screen_of_three_byte_characters_past_the_cap_is_not_cut_inside_one() {
        // The cap counts characters while `len()` counts bytes, so the cut
        // has to be a character count too. `é` is two bytes, and `len() -
        // cap` lands on a boundary by accident for it, so the two-byte case
        // above cannot see this one: `€` is three, and 65536 is not a
        // multiple of three. This screen is 21840 three-byte characters
        // followed by six full buffers of `x`; the first trim happens on the
        // sixth of those, with `len() - cap` at 49136 — inside the
        // three-byte run, two bytes past a boundary, which is exactly where
        // a byte-indexed drain panics instead of trimming.
        let mut reads = 0;
        let mut reader = ScriptedReader(|buf: &mut [u8]| {
            reads += 1;
            match reads {
                1..=8 => {
                    for (i, byte) in buf[..2730 * 3].iter_mut().enumerate() {
                        *byte = match i % 3 {
                            0 => 0xe2,
                            1 => 0x82,
                            _ => 0xac,
                        };
                    }
                    Ok(2730 * 3)
                }
                9..=14 => {
                    buf.fill(b'x');
                    Ok(buf.len())
                }
                _ => Ok(0),
            }
        });
        let screen = drain(&mut reader).expect("a full read buffer is not an error");
        assert_eq!(screen.chars().count(), SCREEN_CAP_CHARS);
        assert_eq!(
            screen.len(),
            98_304,
            "16384 three-byte characters and 49152 bytes of `x`"
        );
    }

    /// Drive a real child through the terminal the driver reads, on a thread
    /// so that a call which never comes back fails a timeout here instead of
    /// hanging the whole suite.
    fn drive_a_real_child(argv: &[&str], budget: Duration) -> (LaunchOutcome, Duration) {
        let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
        let cwd = std::env::current_dir().expect("the test process has a working directory");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let terminal = ScriptTerminal::spawn(&argv, &cwd, &[]).expect("script must start");
            let terminal = std::cell::RefCell::new(terminal);
            let started = Instant::now();
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
                false,
                budget,
                &mut Instant::now,
            );
            let _ = tx.send((outcome, started.elapsed()));
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the driver must come back instead of waiting on the child")
    }

    #[test]
    fn a_child_that_says_nothing_and_never_exits_ends_the_drive_at_its_budget() {
        // `sleep` writes nothing and outlives the budget by a wide margin, so
        // the launch has to end on the clock rather than on the child's next
        // word or on its exit.
        let (outcome, elapsed) = drive_a_real_child(&["sleep", "30"], Duration::from_millis(500));
        assert!(!outcome.clean, "{outcome:?}");
        assert_eq!(
            outcome.blocker,
            Some("no verified interface before the launch budget ran out")
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "a 500 ms budget took {elapsed:?}"
        );
    }

    #[test]
    fn a_child_that_never_stops_writing_ends_the_drive_at_its_budget() {
        // A stream the reader can never catch up with is the other shape that
        // holds a pump open; the driver still gets back to its own clock, and
        // the screen it read is the one it reports.
        let (outcome, elapsed) = drive_a_real_child(&["yes"], Duration::from_millis(500));
        assert!(!outcome.screen.is_empty(), "{outcome:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "a 500 ms budget took {elapsed:?}"
        );
    }

    #[test]
    fn the_bytes_sent_to_the_child_come_back_through_the_pump() {
        // `cat` echoes its stdin, so the marker on the screen is proof the
        // bytes left this process: a `send` that wrote nothing would leave
        // the driver typing into a terminal nothing ever sees.
        let argv = ["cat".to_string()];
        let cwd = std::env::current_dir().expect("the test process has a working directory");
        let mut terminal = ScriptTerminal::spawn(&argv, &cwd, &[]).expect("script must start");
        terminal
            .send(b"hello-from-the-test\r")
            .expect("the child has a stdin");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut screen = String::new();
        while Instant::now() < deadline && !screen.contains("hello-from-the-test") {
            screen = terminal
                .pump()
                .expect("the pty stays readable while the child lives")
                .to_string();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(screen.contains("hello-from-the-test"), "{screen:?}");
    }

    #[test]
    fn a_child_that_only_speaks_after_a_moment_still_settles_clean() {
        // The budget is measured after the screen is read, so a child that is
        // silent for the first polls and then prints is a launch that
        // settled, not one that ran out of time: a budget checked before the
        // screen would report the interface that never appeared, one poll in.
        let (outcome, _) = drive_a_real_child(
            &["sh", "-c", "sleep 0.3; echo ready"],
            Duration::from_secs(2),
        );
        assert!(outcome.clean, "{outcome:?}");
        assert_eq!(outcome.blocker, None, "{outcome:?}");
        assert!(outcome.screen.contains("ready"), "{outcome:?}");
    }

    #[test]
    fn a_child_the_driver_gave_up_on_is_reaped_with_the_terminal() {
        // The terminal owns the teardown, and the launch budget ends with the
        // child still running: a child killed but never waited on would sit as
        // a zombie for the life of the process.
        let argv = ["sleep".to_string(), "30".to_string()];
        let cwd = std::env::current_dir().expect("the test process has a working directory");
        let pid = {
            let terminal = ScriptTerminal::spawn(&argv, &cwd, &[]).expect("script must start");
            terminal.child.id()
        };
        let pid = libc::pid_t::try_from(pid).expect("a pid fits in a pid_t");
        // SAFETY: the pid named a child of this test a moment ago and the
        // terminal has since waited on it; signal 0 delivers nothing and only
        // asks whether the process is still there.
        let gone = unsafe { libc::kill(pid, 0) };
        assert_eq!(gone, -1, "the child outlived the terminal that owned it");
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "the child is a zombie rather than gone"
        );
    }

    /// A child torn down by the terminal once it has reached the screen, and
    /// the status the teardown left behind: the status is what tells an exit
    /// the child chose from one it was given.
    fn teardown_status(argv: &[&str], ready: &str) -> ExitStatus {
        let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
        let cwd = std::env::current_dir().expect("the test process has a working directory");
        let mut terminal = ScriptTerminal::spawn(&argv, &cwd, &[]).expect("script must start");
        // The interrupt has to arrive after the process it is meant for: the
        // byte is read by the pty's line discipline, which signals whoever is
        // the terminal's foreground group at that moment, and a byte written
        // before the child has taken the pty signals nobody.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut screen = String::new();
        while Instant::now() < deadline && !screen.contains(ready) {
            screen = terminal
                .pump()
                .expect("the pty stays readable while the child lives")
                .to_string();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(screen.contains(ready), "the child never spoke: {screen:?}");
        terminal.stop();
        terminal
            .child
            .wait()
            .expect("the teardown reaps the child it stopped")
    }

    #[test]
    fn the_teardown_lets_the_child_act_on_the_interrupt_before_the_kill() {
        // A child that leaves on the interrupt with a status of its own is
        // distinguishable from one that had to be killed, and the grace
        // period is exactly that difference: a teardown that escalated to the
        // kill straight away reports SIGKILL instead of the status the child
        // chose. The trap takes its time on purpose, so the child is still
        // alive when a teardown that did not wait for it kills it.
        let status = teardown_status(
            &[
                "sh",
                "-c",
                "trap 'sleep 0.2; exit 7' INT; echo ready; sleep 5",
            ],
            "ready",
        );
        let code = status.code();
        let signal = status.signal();
        assert_eq!(code, Some(7), "code {code:?}, signal {signal:?}");
    }
}
