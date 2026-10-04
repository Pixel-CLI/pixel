// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The four agents: the argv of each one's functional probe, and what its
//! output means.
//!
//! Every command here is the one the TypeScript stack at
//! `recording-readiness` runs, flag for flag, except where a flag is called
//! out as this port's own. Each agent is asked the same thing — reply
//! `READY` without touching anything — because that single round trip is the
//! only evidence that distinguishes "the config points somewhere" from "the
//! agent can actually reach a model".

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// The prompt every agent probe sends. The second sentence is load-bearing:
/// without it a probe can pass by running a tool, which proves the agent
/// started, not that a model answered.
pub(crate) const PROBE_PROMPT: &str =
    "Reply exactly READY. Do not use tools, execute commands, or modify files.";

/// The token a genuinely-completed round trip carries back.
pub(crate) const READY_TOKEN: &str = "READY";

/// Claude Code's model when nothing names one, matching the reference.
pub(crate) const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4-6";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum AgentFlag {
    Codex,
    Claude,
    Antigravity,
    Devin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Agent {
    Codex,
    Claude,
    Antigravity,
    Devin,
}

impl Agent {
    pub(crate) const ALL: [Agent; 4] = [Self::Codex, Self::Claude, Self::Antigravity, Self::Devin];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Devin => "devin",
        }
    }

    /// The executable the agent installs under. Antigravity's is `agy`.
    pub(crate) const fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "agy",
            Self::Devin => "devin",
        }
    }

    /// Devin's config is verified, never rewritten: the reference does not
    /// repoint it, and its model is a Devin-side identifier (`swe-2-medium`)
    /// that no provider list here has an equivalent for.
    pub(crate) const fn rewrites_config(self) -> bool {
        !matches!(self, Self::Devin)
    }
}

impl From<AgentFlag> for Agent {
    fn from(flag: AgentFlag) -> Self {
        match flag {
            AgentFlag::Codex => Self::Codex,
            AgentFlag::Claude => Self::Claude,
            AgentFlag::Antigravity => Self::Antigravity,
            AgentFlag::Devin => Self::Devin,
        }
    }
}

/// The argv that runs one agent's functional probe.
///
/// `minimal` drops what the probe does not need. For Claude that is the
/// system prompt itself: a readiness round trip needs the model to answer,
/// not the harness to re-explain the world, and the prompt is the largest
/// thing the request carries. `--system-prompt` replaces the default
/// outright, which is the only lever that removes it rather than adding to
/// it.
pub(crate) fn probe_argv(
    agent: Agent,
    prompt: &str,
    claude_model: &str,
    print_timeout: &str,
    export_path: &str,
    minimal: bool,
) -> Vec<String> {
    let owned = str::to_string;
    // One spelling for the binary. `executable` is the accessor for it, and a
    // copy of the name here is what drifts the day the accessor moves — the
    // test that says every lane runs its own binary cannot see a copy.
    let exe = agent.executable();
    match agent {
        // `exec` is Codex's non-interactive path; `-s read-only` bounds the
        // probe so a model that ignores the prompt still cannot write.
        Agent::Codex => [
            exe,
            "--no-daemon",
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "-s",
            "read-only",
            prompt,
        ]
        .iter()
        .map(|s| owned(s))
        .collect(),
        Agent::Claude => {
            // `--safe-mode` keeps the project's CLAUDE.md, skills, plugins,
            // hooks and MCP servers from loading, so the probe measures the
            // CLI's route rather than whatever a repository configured on top
            // of it. `--restricted` drops the tools that run commands or code
            // and confines the file tools to the working directory. `--tools
            // ""` is the one real guard — neither of the other two is a write
            // boundary by itself. The reference's comment above this argv
            // still reads "No `--bare`/safe-mode: preserve normal settings"
            // while the argv passes `--safe-mode`; the argv is the half that
            // is current.
            let mut argv: Vec<String> = [
                exe,
                "-p",
                prompt,
                "--model",
                claude_model,
                "--safe-mode",
                "--restricted",
                "--tools",
                "",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
            ]
            .iter()
            .map(|s| owned(s))
            .collect();
            if minimal {
                argv.push("--system-prompt".to_string());
                argv.push(String::new());
            }
            argv
        }
        Agent::Antigravity => [
            exe,
            "-p",
            prompt,
            "--output-format",
            "stream-json",
            "--print-timeout",
            print_timeout,
        ]
        .iter()
        .map(|s| owned(s))
        .collect(),
        // `--permission-mode auto` is the one auto-approval this probe needs
        // and the only one it takes: the reference's comment on it is that
        // Devin has no additive `--tools`/`--policy` override, and
        // `--config` would replace the user's settings, so the probe runs
        // with the workspace's own settings and is judged afterwards on the
        // trajectory instead. It approves tool prompts for a process whose
        // filesystem is read-only, not a trust decision that outlives the
        // run — which is why it is here and the trust writes are behind
        // `--approve`.
        //
        // `--export` writes the trajectory, which is what makes Devin's
        // result inspectable rather than a bare exit code.
        Agent::Devin => [
            exe,
            "--permission-mode",
            "auto",
            "--export",
            export_path,
            "-p",
            prompt,
        ]
        .iter()
        .map(|s| owned(s))
        .collect(),
    }
}

/// Devin's own auth check, which the reference treats as the readiness
/// signal for it (success iff the output starts a line with `Logged in`).
pub(crate) fn devin_auth_argv() -> Vec<String> {
    [Agent::Devin.executable(), "auth", "status"]
        .iter()
        .map(|s| str::to_string(s))
        .collect()
}

/// What one agent's probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentProbe {
    pub(crate) ready: bool,
    /// The reply, or the classified reason it did not arrive.
    pub(crate) detail: String,
    /// The blocker keyword that stopped it, when the output named one.
    pub(crate) blocker: Option<&'static str>,
}

impl AgentProbe {
    pub(crate) const fn ready(detail: String) -> Self {
        Self {
            ready: true,
            detail,
            blocker: None,
        }
    }

    pub(crate) const fn failed(detail: String) -> Self {
        Self {
            ready: false,
            detail,
            blocker: None,
        }
    }

    pub(crate) const fn blocked(blocker: &'static str, detail: String) -> Self {
        Self {
            ready: false,
            detail,
            blocker: Some(blocker),
        }
    }
}

/// What an agent's output says stopped it. A classification, not a sentence:
/// the sentence depends on facts the output does not carry. See
/// [`FailureClass::reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The account's budget is spent — not a throttle, a wall.
    Quota,
    /// Throttled; the same request may work later.
    RateLimited,
    /// The CLI rejected the credential. Whether there *was* one is not
    /// something the output says.
    Auth,
}

impl FailureClass {
    /// The wording for this class. `Auth` is the arm that needs a second
    /// fact: Claude Code prints the same `authentication_failed` token for a
    /// credential that was rejected and for one that was never configured,
    /// so "expired" — which asserts there was a credential and it lapsed —
    /// is true of at most one of those. `credential_configured` is what
    /// tells them apart, and it comes from the environment, not the stream.
    pub(crate) const fn reason(self, credential_configured: bool) -> &'static str {
        match self {
            Self::Quota => "quota or credits exhausted",
            Self::RateLimited => "rate limited",
            Self::Auth if credential_configured => "credential rejected",
            Self::Auth => "no credential configured",
        }
    }
}

/// The reference's `classifyFailure`, kept as its own function so the three
/// conditions stay ordered: quota before rate limit, and both before the
/// catch-all. A body that says "usage limit reached" and "429" is exhausted,
/// not merely throttled, and the order is what decides.
pub(crate) fn classify_output(text: &str) -> Option<FailureClass> {
    static QUOTA: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)insufficient.quota|quota.{0,30}(exhaust|exceed)|usage limit.{0,30}(reach|exceed)|out of (credits|usage)|credit balance.{0,30}(low|insufficient)|budget.{0,20}exceed",
        )
        .unwrap()
    });
    static RATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)rate.?limit|too many requests|\b429\b").unwrap());
    static AUTH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)unauthori[sz]ed|authentication.*(fail|invalid|expired)|token.*expired|\b401\b|not logged in",
        )
        .unwrap()
    });
    if QUOTA.is_match(text) {
        return Some(FailureClass::Quota);
    }
    if RATE.is_match(text) {
        return Some(FailureClass::RateLimited);
    }
    if AUTH.is_match(text) {
        return Some(FailureClass::Auth);
    }
    None
}

/// The env vars that carry Claude's gateway credential. Claude Code prints
/// the same `authentication_failed` token for a credential that was rejected
/// and for one that was never configured, so the report reads the
/// environment to tell the two apart.
///
/// `ANTHROPIC_API_KEY` is deliberately absent. Every child this command
/// spawns runs with it removed ([`crate::ai_cli_readify::run_capture`],
/// `terminal`, `auth`, `rpc`), so a shell that exports it does not put a
/// credential in front of the agent — counting it here would report the
/// opposite of what the probe passed through, and a run with only that
/// variable set would read as a rejected credential instead of a missing
/// one.
pub(crate) const CLAUDE_CREDENTIAL_ENVS: [&str; 2] =
    ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_CUSTOM_HEADERS"];

/// True when any of `names` is set to something that is not blank, which is
/// all the report needs: a name present but empty is not a credential.
///
/// The process environment is the one input a test cannot arrange — emptying
/// it is a write to state every other test in this binary shares, and they
/// run in parallel — so the read is kept this thin and the rule below it is
/// asserted through [`any_non_blank`], which is handed the values instead.
#[cfg_attr(test, mutants::skip)] // reads the process environment; any_non_blank asserts the rule
pub(crate) fn any_env_set(names: &[&str]) -> bool {
    any_non_blank(names.iter().map(|name| std::env::var(name).ok()))
}

/// A name counts only when it carries something. `export ANTHROPIC_AUTH_TOKEN=`
/// sets the name and asserts nothing, and the report must not call that a
/// configured credential; the names are alternatives, so one is enough.
fn any_non_blank(mut values: impl Iterator<Item = Option<String>>) -> bool {
    values.any(|value| value.is_some_and(|value| !value.trim().is_empty()))
}

/// Every string a JSONL stream carries under a text-shaped key, concatenated.
/// Deliberately shape-tolerant: Codex, Claude and Antigravity each wrap a
/// message in their own event type, and the probe cares about the words, not
/// the envelope. A line that is not JSON is kept whole, so a plain-text
/// agent's output still reaches the classifier.
///
/// The key list has to name every name an agent puts its words under, and
/// missing one is silent: Antigravity wraps its reply as
/// `{"event":"result","result":{…}}`, so the text arrives under
/// `text_delta` and `response` — neither of which was listed, which dropped
/// a successful `READY` and reported the agent as having reached no model.
pub(crate) fn collect_text(stream: &str) -> String {
    let mut out = String::new();
    for line in stream.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                walk_strings(&value, &mut |key, text| {
                    if matches!(
                        key,
                        "text" | "content" | "result" | "text_delta" | "response"
                    ) {
                        out.push_str(text);
                        out.push('\n');
                    }
                });
            }
            Err(_) => {
                out.push_str(trimmed);
                out.push('\n');
            }
        }
    }
    out
}

/// Every string a JSONL stream carries under an error-shaped key.
pub(crate) fn collect_errors(stream: &str) -> String {
    let mut out = String::new();
    for line in stream.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        walk_strings(&value, &mut |key, text| {
            if matches!(key, "error" | "message" | "detail") {
                out.push_str(text);
                out.push('\n');
            }
        });
    }
    out
}

/// Call `visit` for every string in `value`, with the key it sat under.
fn walk_strings(value: &Value, visit: &mut impl FnMut(&str, &str)) {
    match value {
        Value::String(text) => visit("", text),
        Value::Array(items) => {
            for item in items {
                walk_strings(item, visit);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                match item {
                    Value::String(text) => visit(key, text),
                    other => walk_strings(other, visit),
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Decide what one probe's output means.
///
/// Ready requires all three: the process exited 0, the output carries the
/// reply token, and nothing in the output names a limit or an expired
/// credential. The last condition is what catches the case this whole module
/// exists for — an agent that reports success in its exit code while the
/// stream carries a rate-limit notice.
pub(crate) fn parse_probe(stream: &str, success: bool) -> AgentProbe {
    parse_probe_with(stream, success, true)
}

/// [`parse_probe`], told whether a credential was configured.
///
/// The default reader assumes one was, which is the wording that does not
/// invent a missing credential. The Claude lane passes what the environment
/// actually holds, because its CLI answers the same way either way.
pub(crate) fn parse_probe_with(
    stream: &str,
    success: bool,
    credential_configured: bool,
) -> AgentProbe {
    let text = collect_text(stream);
    let errors = collect_errors(stream);
    let combined = format!("{text}\n{errors}");
    if let Some(class) = classify_output(&combined) {
        let reason = class.reason(credential_configured);
        return AgentProbe::failed(format!("{reason}: {}", first_line(&errors, &text)));
    }
    if !success {
        return AgentProbe::failed(format!(
            "probe exited non-zero: {}",
            first_line(&errors, &text)
        ));
    }
    if !text.contains(READY_TOKEN) {
        return AgentProbe::failed(format!(
            "probe reached no model: {}",
            first_line(&errors, &text)
        ));
    }
    AgentProbe::ready(first_line(&errors, &text))
}

/// The line `devin auth status` prints when it is logged in. Matched at the
/// start of a line, not as a substring, so an error that merely mentions
/// being logged in does not pass.
const DEVIN_LOGGED_IN: &str = "Logged in";

/// Decide what Devin's readiness check means.
///
/// Devin is the one agent whose readiness is not a model round trip: its
/// lane runs `devin auth status` (see [`devin_auth_argv`]), which sends no
/// prompt and therefore never carries the reply token. Feeding that output
/// to [`parse_probe`] reported every authenticated Devin as "probe reached
/// no model", on the strength of a line that said it was logged in.
/// Readiness here is the exit code plus the line, which is what
/// [`devin_auth_argv`] has documented since it was written.
pub(crate) fn parse_devin_auth(stream: &str, success: bool) -> AgentProbe {
    let text = collect_text(stream);
    let errors = collect_errors(stream);
    let combined = format!("{text}\n{errors}");
    if let Some(class) = classify_output(&combined) {
        let reason = class.reason(true);
        return AgentProbe::failed(format!("{reason}: {}", first_line(&errors, &text)));
    }
    if !success {
        return AgentProbe::failed(format!(
            "auth status exited non-zero: {}",
            first_line(&errors, &text)
        ));
    }
    if !text
        .lines()
        .any(|line| line.trim_start().starts_with(DEVIN_LOGGED_IN))
    {
        return AgentProbe::failed(format!("not logged in: {}", first_line(&errors, &text)));
    }
    AgentProbe::ready(first_line(&errors, &text))
}

/// The first non-empty line of `errors`, else of `text`, else a placeholder.
/// One line keeps a report row one row.
fn first_line(errors: &str, text: &str) -> String {
    for source in [errors, text] {
        if let Some(line) = source.lines().find(|l| !l.trim().is_empty()) {
            return line.trim().chars().take(180).collect();
        }
    }
    "(no output)".to_string()
}

/// Mark a probe that a startup prompt stopped before it could run. Kept
/// apart from [`parse_probe`] because the difference matters to the report:
/// a blocked agent never reached a provider, so it says nothing about the
/// provider's health.
pub(crate) fn blocked_by(blocker: &'static str, detail: &str) -> AgentProbe {
    AgentProbe::blocked(blocker, detail.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(agent: Agent, minimal: bool) -> Vec<String> {
        probe_argv(
            agent,
            PROBE_PROMPT,
            DEFAULT_CLAUDE_MODEL,
            "20s",
            "/tmp/t.json",
            minimal,
        )
    }

    #[test]
    fn every_agent_is_probed_with_the_same_prompt() {
        for agent in Agent::ALL {
            let argv = argv(agent, true);
            assert!(
                argv.iter().any(|a| a == PROBE_PROMPT),
                "{agent:?} never receives the prompt: {argv:?}"
            );
        }
    }

    #[test]
    fn every_agent_probes_through_its_own_executable() {
        assert_eq!(argv(Agent::Codex, true)[0], "codex");
        assert_eq!(argv(Agent::Claude, true)[0], "claude");
        assert_eq!(argv(Agent::Antigravity, true)[0], "agy");
        assert_eq!(argv(Agent::Devin, true)[0], "devin");
    }

    #[test]
    fn codex_probes_non_interactively_and_bounded() {
        let argv = argv(Agent::Codex, true);
        assert_eq!(argv[1], "--no-daemon");
        assert_eq!(argv[2], "exec");
        assert!(argv.contains(&"--json".to_string()), "{argv:?}");
        assert!(argv.contains(&"--ephemeral".to_string()), "{argv:?}");
        // The sandbox flag and its value sit immediately before the prompt,
        // which is the last argument.
        assert_eq!(argv[argv.len() - 3], "-s", "{argv:?}");
        assert_eq!(argv[argv.len() - 2], "read-only", "{argv:?}");
        assert_eq!(argv[argv.len() - 1], PROBE_PROMPT, "{argv:?}");
    }

    #[test]
    fn claude_probes_in_print_mode_with_streaming_json() {
        let argv = argv(Agent::Claude, true);
        assert!(argv.contains(&"-p".to_string()), "{argv:?}");
        assert!(argv.contains(&"stream-json".to_string()), "{argv:?}");
        assert!(argv.contains(&"--verbose".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"--no-session-persistence".to_string()),
            "{argv:?}"
        );
        assert!(argv.contains(&DEFAULT_CLAUDE_MODEL.to_string()), "{argv:?}");
    }

    #[test]
    fn the_claude_probe_carries_its_isolation_flags() {
        let argv = argv(Agent::Claude, true);
        assert!(argv.contains(&"--safe-mode".to_string()), "{argv:?}");
        assert!(argv.contains(&"--restricted".to_string()), "{argv:?}");
        // `--tools` and its value stay adjacent, and the value is empty. It is
        // the only flag here that really disables the tools: a pair that came
        // apart would hand `--tools` the next flag as its value, which is
        // either a usage error or every tool enabled — never the guard.
        let at = argv
            .iter()
            .position(|a| a == "--tools")
            .expect("the probe must restrict the tools");
        assert_eq!(argv[at + 1], "", "an empty tool list disables them all");
    }

    #[test]
    fn the_minimal_claude_probe_replaces_the_system_prompt() {
        let argv = argv(Agent::Claude, true);
        let at = argv
            .iter()
            .position(|a| a == "--system-prompt")
            .expect("minimal probe must replace the system prompt");
        assert_eq!(argv[at + 1], "", "the replacement must be empty");
    }

    #[test]
    fn the_full_claude_probe_keeps_the_default_system_prompt() {
        let argv = argv(Agent::Claude, false);
        assert!(
            !argv.contains(&"--system-prompt".to_string()),
            "the full probe must not pass the flag at all: {argv:?}"
        );
    }

    #[test]
    fn antigravity_carries_its_own_print_timeout() {
        let argv = probe_argv(
            Agent::Antigravity,
            PROBE_PROMPT,
            DEFAULT_CLAUDE_MODEL,
            "35s",
            "/tmp/t.json",
            true,
        );
        let at = argv.iter().position(|a| a == "--print-timeout").unwrap();
        assert_eq!(argv[at + 1], "35s");
        assert_eq!(argv[1], "-p");
    }

    #[test]
    fn devin_exports_its_trajectory() {
        let argv = probe_argv(
            Agent::Devin,
            PROBE_PROMPT,
            DEFAULT_CLAUDE_MODEL,
            "20s",
            "/scratch/trajectory.json",
            true,
        );
        let at = argv.iter().position(|a| a == "--export").unwrap();
        assert_eq!(argv[at + 1], "/scratch/trajectory.json");
        let mode = argv
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap_or_else(|| panic!("Devin's probe must carry a permission mode: {argv:?}"));
        assert_eq!(
            argv[mode + 1],
            "auto",
            "the reference's mode is `auto`; any other value stops the probe at a tool prompt instead: {argv:?}"
        );
    }

    #[test]
    fn devin_is_the_only_agent_whose_config_is_not_rewritten() {
        assert!(!Agent::Devin.rewrites_config());
        for agent in [Agent::Codex, Agent::Claude, Agent::Antigravity] {
            assert!(agent.rewrites_config(), "{agent:?}");
        }
    }

    #[test]
    fn a_clean_ready_reply_is_ready() {
        let stream = r#"{"type":"item.completed","item":{"type":"agent_message","text":"READY"}}"#;
        let probe = parse_probe(stream, true);
        assert!(probe.ready, "{probe:?}");
        assert_eq!(probe.detail, "READY");
    }

    #[test]
    fn a_reply_buried_in_an_envelope_is_found() {
        let stream =
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"READY"}]}}"#;
        assert!(parse_probe(stream, true).ready);
    }

    #[test]
    fn plain_text_output_is_read_too() {
        assert!(parse_probe("READY\n", true).ready);
    }

    #[test]
    fn a_zero_exit_with_no_reply_is_not_ready() {
        let probe = parse_probe(r#"{"type":"session.created"}"#, true);
        assert!(!probe.ready, "{probe:?}");
        assert!(probe.detail.contains("reached no model"), "{probe:?}");
    }

    #[test]
    fn a_non_zero_exit_is_not_ready_even_with_the_token() {
        let probe = parse_probe("READY", false);
        assert!(!probe.ready, "{probe:?}");
        assert!(probe.detail.contains("non-zero"), "{probe:?}");
    }

    #[test]
    fn a_rate_limit_in_the_stream_beats_a_zero_exit() {
        // The case the honest probe exists for: exit 0, and the stream says
        // the provider throttled it.
        let stream = r#"{"type":"error","message":"429 Too many requests"}"#;
        let probe = parse_probe(stream, true);
        assert!(!probe.ready, "{probe:?}");
        assert!(probe.detail.contains("rate limited"), "{probe:?}");
    }

    #[test]
    fn an_exhausted_quota_is_named_as_quota_not_as_a_rate_limit() {
        let probe = parse_probe(
            r#"{"error":{"message":"insufficient_quota: usage limit reached"}}"#,
            true,
        );
        assert!(!probe.ready);
        assert!(
            probe.detail.contains("quota or credits exhausted"),
            "{probe:?}"
        );
        assert!(!probe.detail.contains("rate limited"), "{probe:?}");
    }

    #[test]
    fn a_rejected_credential_is_named_as_rejected() {
        // The default reader assumes a credential was there, which is the
        // wording that does not invent a missing one.
        let probe = parse_probe(r#"{"message":"401 unauthorized"}"#, false);
        assert!(probe.detail.contains("credential rejected"), "{probe:?}");
        assert!(
            !probe.detail.contains("expired"),
            "the output says the credential failed, not that it lapsed: {probe:?}"
        );
    }

    #[test]
    fn the_same_authentication_failure_reads_differently_when_nothing_was_configured() {
        // Claude Code prints `authentication_failed` for a credential that
        // was rejected and for one that was never configured. "Expired"
        // asserts a credential lapsed, which with none configured sends the
        // reader to a login prompt that is not the problem.
        let stream = r#"{"error":"authentication_failed"}"#;
        let configured = parse_probe_with(stream, false, true);
        let absent = parse_probe_with(stream, false, false);
        assert!(
            configured.detail.contains("credential rejected"),
            "{configured:?}"
        );
        assert!(
            absent.detail.contains("no credential configured"),
            "{absent:?}"
        );
        assert_ne!(configured.detail, absent.detail);
    }

    #[test]
    fn a_blank_credential_variable_is_not_a_credential() {
        // `export ANTHROPIC_AUTH_TOKEN=` sets the name and asserts nothing.
        // Reading that as configured is how a run reports a credential the
        // user never supplied.
        assert!(
            !any_non_blank([Some(String::new())].into_iter()),
            "an empty value is not a credential"
        );
        assert!(
            !any_non_blank([Some("   ".to_string())].into_iter()),
            "whitespace is not a credential either"
        );
        assert!(
            !any_non_blank([None].into_iter()),
            "an unset name is not a credential"
        );
        assert!(
            !any_non_blank(std::iter::empty()),
            "no names at all is not a credential"
        );
    }

    #[test]
    fn one_credential_among_the_alternatives_is_enough() {
        // The names are alternatives a user picks one of, so the rule is
        // `any` and never `all`.
        let values = [None, Some("token".to_string()), Some(String::new())];
        assert!(
            any_non_blank(values.into_iter()),
            "one set name among unset ones is a configured credential"
        );
    }

    #[test]
    fn the_credential_list_holds_only_what_the_probe_passes_through() {
        // Every child this command spawns runs with `ANTHROPIC_API_KEY`
        // removed, so its presence in the shell cannot be what the agent saw.
        // Listing it would invert the report's own distinction: a run whose
        // only variable is the stripped one would read as a rejected
        // credential where the truth is that none reached the agent.
        assert_eq!(
            CLAUDE_CREDENTIAL_ENVS,
            ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_CUSTOM_HEADERS"]
        );
    }

    #[test]
    fn an_antigravity_result_envelope_reaches_the_parser() {
        // The shape `agy --output-format stream-json` really emits: the reply
        // sits under `text_delta` and `response`, neither of which was a
        // listed key, so a successful READY was dropped and the agent was
        // reported as having reached no model.
        let stream = concat!(
            r#"{"event":"step_update","step_update":{"text_delta":"READY\n"}}"#,
            "\n",
            r#"{"event":"result","result":{"response":"READY\n","status":"SUCCESS"}}"#,
            "\n",
        );
        let probe = parse_probe(stream, true);
        assert!(probe.ready, "{probe:?}");
    }

    #[test]
    fn devin_is_ready_on_its_auth_status_which_carries_no_reply_token() {
        // The line `devin auth status` actually prints, verbatim.
        let probe = parse_devin_auth("Logged in (via Devin).\nCredentials: stored\n", true);
        assert!(probe.ready, "{probe:?}");
        assert!(probe.detail.contains("Logged in"), "{probe:?}");
    }

    #[test]
    fn devin_is_not_ready_when_its_auth_status_says_nothing_is_stored() {
        // Chosen because it trips none of the three classifier patterns, so
        // this reaches the not-logged-in branch rather than the auth one.
        let probe = parse_devin_auth("No credentials found for this account.\n", true);
        assert!(!probe.ready, "{probe:?}");
        assert!(
            probe.detail.contains("not logged in"),
            "the reason has to be in the row: {probe:?}"
        );
    }

    #[test]
    fn devin_is_not_ready_when_its_auth_check_itself_fails() {
        let probe = parse_devin_auth("Logged in (via Devin).\n", false);
        assert!(
            !probe.ready,
            "an exit code that is not 0 outranks a line that says logged in: {probe:?}"
        );
        assert!(probe.detail.contains("non-zero"), "{probe:?}");
    }

    #[test]
    fn a_gateway_refusal_with_no_keyword_is_reported_by_its_own_words() {
        // "No connected db" matches none of the three classes; the detail
        // must still carry what the gateway said.
        let probe = parse_probe(r#"{"error":{"message":"No connected db"}}"#, true);
        assert!(!probe.ready, "{probe:?}");
        assert!(probe.detail.contains("No connected db"), "{probe:?}");
    }

    #[test]
    fn classify_prefers_quota_over_rate_limit_when_both_appear() {
        let text = "429 Too many requests; usage limit reached";
        assert_eq!(classify_output(text), Some(FailureClass::Quota));
    }

    #[test]
    fn classify_is_silent_on_an_ordinary_failure() {
        assert_eq!(classify_output("model not found"), None);
    }

    #[test]
    fn the_detail_is_one_short_line() {
        let long = "e".repeat(500);
        let probe = parse_probe(&format!(r#"{{"error":{{"message":"{long}"}}}}"#), true);
        assert!(!probe.detail.contains('\n'), "{probe:?}");
        // The quoted message is capped at 180 characters; the classified
        // reason and its separator ride in front of it, so the row's whole
        // budget is that cap plus the longest label this module can emit.
        assert!(
            probe.detail.chars().count() <= 240,
            "{} chars: {}",
            probe.detail.chars().count(),
            probe.detail
        );
        // The 500-character message must reach the row truncated: the
        // longest unbroken run of it is the 180-character cap, not the whole
        // thing. Counting `e`s anywhere in the line would also count the
        // ones in the reason in front of it.
        assert!(
            !probe.detail.contains(&"e".repeat(181)),
            "the message was not truncated to 180 characters: {}",
            probe.detail.chars().count()
        );
        assert!(
            probe.detail.contains(&"e".repeat(180)),
            "the message was truncated short of the cap: {}",
            probe.detail.chars().count()
        );
    }

    #[test]
    fn a_blocked_probe_says_which_blocker_stopped_it() {
        let probe = blocked_by("workspace trust", "Do you trust this folder?");
        assert!(!probe.ready);
        assert_eq!(probe.blocker, Some("workspace trust"));
        assert_eq!(probe.detail, "Do you trust this folder?");
    }

    #[test]
    fn devin_auth_is_asked_of_devin_itself() {
        assert_eq!(devin_auth_argv(), vec!["devin", "auth", "status"]);
    }
}
