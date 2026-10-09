// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which coding agent is driving this process, from the environment
//! variables the known harnesses set when they spawn a shell command.
//!
//! Ported from GitButler's `but` (`crates/but/src/utils/detect_agent.rs`,
//! read at `5cbe33d`), which follows the `@vercel/detect-agent` convention.
//! The precedence is the contract, not an implementation detail:
//!
//! 1. the generic `AI_AGENT` variable, the cross-harness convention;
//! 2. tool-specific markers, most specific first, so a fork's marker beats
//!    the upstream marker it inherits (Kilo Code is an OpenCode fork);
//! 3. the shorter generic `AGENT`, last and only on an allowlist, because a
//!    generic name is as likely to be stale or inherited from the user's
//!    shell as it is to name the agent running now.
//!
//! A value we do not recognise still resolves to [`Agent::Unknown`]: setting
//! `AI_AGENT` is an explicit "an agent is driving this", and the setup wizard
//! needs to know that even when it cannot name the agent.

use std::env;
use std::ffi::OsString;

macro_rules! environment_variables {
    ($( $name:ident = $value:literal ),+ $(,)?) => {
        $(const $name: &str = $value;)+

        /// Every environment variable consulted during agent detection.
        pub const ENVIRONMENT_VARIABLES: &[&str] = &[$($value),+];
    };
}

environment_variables! {
    AI_AGENT = "AI_AGENT",
    AGENT = "AGENT",
    CLAUDECODE = "CLAUDECODE",
    CLAUDE_CODE = "CLAUDE_CODE",
    CLAUDE_CODE_IS_COWORK = "CLAUDE_CODE_IS_COWORK",
    CURSOR_AGENT = "CURSOR_AGENT",
    CURSOR_EXTENSION_HOST_ROLE = "CURSOR_EXTENSION_HOST_ROLE",
    CURSOR_TRACE_ID = "CURSOR_TRACE_ID",
    CODEX_SANDBOX = "CODEX_SANDBOX",
    CODEX_CI = "CODEX_CI",
    CODEX_THREAD_ID = "CODEX_THREAD_ID",
    CODEX_SHELL = "CODEX_SHELL",
    AGENT_DISPLAY_OUT = "AGENT_DISPLAY_OUT",
    AGENT_CONTEXT_OUT = "AGENT_CONTEXT_OUT",
    QWEN_CODE = "QWEN_CODE",
    GEMINI_CLI = "GEMINI_CLI",
    ANTIGRAVITY_AGENT = "ANTIGRAVITY_AGENT",
    COPILOT_AGENT = "COPILOT_AGENT",
    JUNIE_DATA = "JUNIE_DATA",
    JUNIE_SHIM_PATH = "JUNIE_SHIM_PATH",
    KILO_PID = "KILO_PID",
    HERMES_SESSION_ID = "HERMES_SESSION_ID",
    OPENCODE_CLIENT = "OPENCODE_CLIENT",
    OPENCODE = "OPENCODE",
    AUGMENT_AGENT = "AUGMENT_AGENT",
    REPL_ID = "REPL_ID",
    DIRAC_ACTIVE = "DIRAC_ACTIVE",
    CLINE_ACTIVE = "CLINE_ACTIVE",
    ROO_CLI_RUNTIME = "ROO_CLI_RUNTIME",
    ROO_ACTIVE = "ROO_ACTIVE",
    TRAE_AI_SHELL_ID = "TRAE_AI_SHELL_ID",
    TABNINE_CLI = "TABNINE_CLI",
    PI_CODING_AGENT = "PI_CODING_AGENT",
    GOOSE_TERMINAL = "GOOSE_TERMINAL",
    AWS_EXECUTION_ENV = "AWS_EXECUTION_ENV",
    CODEBUDDY_SESSION_ID = "CODEBUDDY_SESSION_ID",
    CODEBUDDY_PROJECT_DIR = "CODEBUDDY_PROJECT_DIR",
    GROK_AGENT = "GROK_AGENT",
    OPENCLAW_SHELL = "OPENCLAW_SHELL",
    DSH_SHELL = "DSH_SHELL",
    PS1 = "PS1",
    PROMPT_COMMAND = "PROMPT_COMMAND",
    OZ_HARNESS = "OZ_HARNESS",
    OZ_RUN_ID = "OZ_RUN_ID",
}

/// An AI coding agent that may be driving the CLI.
///
/// Variants the wizard cannot map to a per-agent setup target still exist
/// here: [`crate::setup::agent::AgentTarget::from_detected`] sends them to the
/// generic `AGENTS.md` target, and detection must not lose the distinction
/// between "Codex" and "an agent I cannot name".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    ClaudeCodeCowork,
    Codex,
    Cursor,
    CursorCli,
    Devin,
    GeminiCli,
    Antigravity,
    OpenCode,
    Pi,
    Amp,
    AmazonQ,
    AntigravityCli,
    Augment,
    Claw,
    Cline,
    CodeBuddy,
    Crush,
    DeepSeekHarness,
    DevinCli,
    Dirac,
    GitHubCopilot,
    GitLabDuoCli,
    Goose,
    GrokBuild,
    Hermes,
    Junie,
    KiloCode,
    KiroCli,
    OpenHands,
    Poolside,
    PulumiNeo,
    QwenCode,
    Replit,
    RooCode,
    TabnineCli,
    Trae,
    V0,
    Warp,
    Unknown,
}

impl Agent {
    /// A short, stable identifier, as recorded in the action log and printed
    /// by the wizard's "(detected)" label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::ClaudeCodeCowork => "claude-code-cowork",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::CursorCli => "cursor-cli",
            Self::Devin => "devin",
            Self::GeminiCli => "gemini-cli",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Amp => "amp",
            Self::AmazonQ => "amazon-q",
            Self::AntigravityCli => "antigravity-cli",
            Self::Augment => "augment",
            Self::Claw => "claw",
            Self::Cline => "cline",
            Self::CodeBuddy => "codebuddy",
            Self::Crush => "crush",
            Self::DeepSeekHarness => "deepseek-harness",
            Self::DevinCli => "devin-cli",
            Self::Dirac => "dirac",
            Self::GitHubCopilot => "github-copilot",
            Self::GitLabDuoCli => "gitlab-duo-cli",
            Self::Goose => "goose",
            Self::GrokBuild => "grok-build",
            Self::Hermes => "hermes",
            Self::Junie => "junie",
            Self::KiloCode => "kilo-code",
            Self::KiroCli => "kiro-cli",
            Self::OpenHands => "openhands",
            Self::Poolside => "poolside",
            Self::PulumiNeo => "pulumi-neo",
            Self::QwenCode => "qwen-code",
            Self::Replit => "replit",
            Self::RooCode => "roo-code",
            Self::TabnineCli => "tabnine-cli",
            Self::Trae => "trae",
            Self::V0 => "v0",
            Self::Warp => "warp",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An unrecognised agent name. [`detect`] never returns this error: it maps
/// an unknown `AI_AGENT` to [`Agent::Unknown`] instead, because the variable
/// being set at all is the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseAgentError;

impl std::fmt::Display for ParseAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unrecognized agent name")
    }
}

impl std::error::Error for ParseAgentError {}

impl std::str::FromStr for Agent {
    type Err = ParseAgentError;

    /// Parse exactly the way detection interprets `AI_AGENT`: normalised, so
    /// `Claude_Code`, `claude code` and `claude-code@2` all resolve to
    /// [`Agent::ClaudeCode`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match_normalized_value(&normalize_agent_value(s)).ok_or(ParseAgentError)
    }
}

/// Detect the agent driving this process, or `None` when it looks like a
/// human is driving it.
pub fn detect() -> Option<Agent> {
    detect_with(|key| env::var_os(key))
}

/// The detection, with the environment lookup injected. Tests pass a map;
/// production passes [`env::var_os`]. A snapshot keeps a test from reading the
/// environment of the machine running it.
pub fn detect_with(lookup: impl Fn(&str) -> Option<OsString>) -> Option<Agent> {
    let is_set = |var: &str| lookup(var).is_some_and(|v| !v.is_empty());
    let is_value =
        |var: &str, expected: &str| lookup(var).is_some_and(|v| v.to_str() == Some(expected));
    let contains = |var: &str, needle: &str| {
        lookup(var).is_some_and(|v| v.to_str().is_some_and(|value| value.contains(needle)))
    };

    if let Some(agent) = parse_ai_agent_var(&lookup) {
        return Some(agent);
    }

    if is_set(CLAUDE_CODE_IS_COWORK) {
        return Some(Agent::ClaudeCodeCowork);
    }
    if is_set(CLAUDE_CODE) || is_set(CLAUDECODE) {
        return Some(Agent::ClaudeCode);
    }
    if is_set(CURSOR_AGENT) || is_value(CURSOR_EXTENSION_HOST_ROLE, "agent-exec") {
        return Some(Agent::CursorCli);
    }
    if is_set(CURSOR_TRACE_ID) {
        return Some(Agent::Cursor);
    }
    if is_set(CODEX_SANDBOX) || is_set(CODEX_CI) || is_set(CODEX_THREAD_ID) || is_set(CODEX_SHELL) {
        return Some(Agent::Codex);
    }
    // Kiro exposes both FIFO paths only while its agent is driving a command.
    if is_set(AGENT_DISPLAY_OUT) && is_set(AGENT_CONTEXT_OUT) {
        return Some(Agent::KiroCli);
    }
    if is_value(QWEN_CODE, "1") {
        return Some(Agent::QwenCode);
    }
    if is_set(GEMINI_CLI) {
        return Some(Agent::GeminiCli);
    }
    if is_set(ANTIGRAVITY_AGENT) {
        return Some(Agent::Antigravity);
    }
    if is_set(COPILOT_AGENT) {
        return Some(Agent::GitHubCopilot);
    }
    if is_set(JUNIE_DATA) || is_set(JUNIE_SHIM_PATH) {
        return Some(Agent::Junie);
    }
    // Kilo is an OpenCode fork, so its own marker has to be read first.
    if is_set(KILO_PID) {
        return Some(Agent::KiloCode);
    }
    if is_set(HERMES_SESSION_ID) {
        return Some(Agent::Hermes);
    }
    if is_set(OPENCODE_CLIENT) || is_set(OPENCODE) {
        return Some(Agent::OpenCode);
    }
    if is_set(AUGMENT_AGENT) {
        return Some(Agent::Augment);
    }
    if is_set(REPL_ID) {
        return Some(Agent::Replit);
    }
    if is_set(DIRAC_ACTIVE) {
        return Some(Agent::Dirac);
    }
    if is_set(CLINE_ACTIVE) {
        return Some(Agent::Cline);
    }
    if is_set(ROO_CLI_RUNTIME) || is_set(ROO_ACTIVE) {
        return Some(Agent::RooCode);
    }
    if is_set(TRAE_AI_SHELL_ID) {
        return Some(Agent::Trae);
    }
    if is_set(TABNINE_CLI) {
        return Some(Agent::TabnineCli);
    }
    if is_set(PI_CODING_AGENT) {
        return Some(Agent::Pi);
    }
    if is_value(GOOSE_TERMINAL, "1") {
        return Some(Agent::Goose);
    }
    // DeepSeek Harness (`dsh`) sets `DSH_SHELL=1` on every shell-tool child
    // and strips inherited `DSH_*` first, so the marker cannot leak across
    // harnesses.
    if is_value(DSH_SHELL, "1") {
        return Some(Agent::DeepSeekHarness);
    }
    // Inherited by nested agents, so a nested agent's own marker above has to
    // win over the outer command runner's.
    if contains(AWS_EXECUTION_ENV, "AmazonQ-For-CLI") {
        return Some(Agent::AmazonQ);
    }
    if is_set(CODEBUDDY_SESSION_ID) || is_set(CODEBUDDY_PROJECT_DIR) {
        return Some(Agent::CodeBuddy);
    }
    if is_value(GROK_AGENT, "1") {
        return Some(Agent::GrokBuild);
    }
    if is_value(OPENCLAW_SHELL, "exec") {
        return Some(Agent::Claw);
    }
    if contains(PS1, "###PS1JSON###") || contains(PROMPT_COMMAND, "###PS1JSON###") {
        return Some(Agent::OpenHands);
    }
    if is_value(OZ_HARNESS, "oz") {
        return Some(Agent::Warp);
    }
    // The run id stays a fallback only when no harness identity is available.
    if !is_set(OZ_HARNESS) && is_set(OZ_RUN_ID) {
        return Some(Agent::Warp);
    }

    parse_agent_var(&lookup)
}

/// Parse the generic `AI_AGENT` variable. A non-empty value we do not
/// recognise still yields [`Agent::Unknown`].
fn parse_ai_agent_var(lookup: &impl Fn(&str) -> Option<OsString>) -> Option<Agent> {
    let val = normalize_agent_value(&lookup(AI_AGENT)?.to_string_lossy());
    if val.is_empty() {
        return None;
    }
    Some(match_normalized_value(&val).unwrap_or(Agent::Unknown))
}

/// Parse the shorter `AGENT` convention (Goose, Amp, Crush, Codex). Only a
/// strict allowlist counts: any other value is ignored rather than reported as
/// [`Agent::Unknown`], so an unrelated `AGENT` in a user's shell never reads as
/// an agent driving the CLI.
fn parse_agent_var(lookup: &impl Fn(&str) -> Option<OsString>) -> Option<Agent> {
    match normalize_agent_value(&lookup(AGENT)?.to_string_lossy()).as_str() {
        "goose" => Some(Agent::Goose),
        "amp" => Some(Agent::Amp),
        "crush" => Some(Agent::Crush),
        "codex" => Some(Agent::Codex),
        _ => None,
    }
}

/// Match a normalised identifier against a known agent, aliases included.
fn match_normalized_value(val: &str) -> Option<Agent> {
    // GitLab decorates Duo CLI with the LSP version between its product names,
    // e.g. `gitlab-lsp_7.17.0__duo-cli`.
    if val.starts_with("gitlab-lsp-") && val.ends_with("-duo-cli") {
        return Some(Agent::GitLabDuoCli);
    }
    match_agent_name(val).or_else(|| match_agent_name_prefix(val))
}

/// Match an identifier whose leading segments name a known agent, tolerating
/// a trailing version decoration without the `@` separator (Claude Code
/// desktop sets e.g. `claude-code_2-1-202_agent`). Segments drop from the end
/// one at a time, so the longest matching prefix wins.
fn match_agent_name_prefix(val: &str) -> Option<Agent> {
    let mut prefix = val;
    while let Some((rest, _)) = prefix.rsplit_once('-') {
        if let Some(agent) = match_agent_name(rest) {
            return Some(agent);
        }
        prefix = rest;
    }
    None
}

/// Map a recognised identifier to an [`Agent`], or `None` when unknown.
fn match_agent_name(val: &str) -> Option<Agent> {
    Some(match val {
        "claude" | "claude-code" => Agent::ClaudeCode,
        "cowork" | "claude-code-cowork" => Agent::ClaudeCodeCowork,
        "cursor" => Agent::Cursor,
        "cursor-cli" => Agent::CursorCli,
        "codex" => Agent::Codex,
        "devin" => Agent::Devin,
        "devin-cli" => Agent::DevinCli,
        "gemini" | "gemini-cli" => Agent::GeminiCli,
        "antigravity" => Agent::Antigravity,
        "antigravity-cli" => Agent::AntigravityCli,
        "opencode" => Agent::OpenCode,
        "pi" => Agent::Pi,
        "amp" => Agent::Amp,
        "amazon-q" | "amazon-q-developer" | "amazon-q-developer-cli" => Agent::AmazonQ,
        "augment" | "augment-cli" => Agent::Augment,
        "cline" => Agent::Cline,
        "codebuddy" | "codebuddy-code" => Agent::CodeBuddy,
        "crush" => Agent::Crush,
        "dsh" | "deepseek" | "deepseek-harness" => Agent::DeepSeekHarness,
        "dirac" => Agent::Dirac,
        "copilot" | "github-copilot" | "github-copilot-cli" | "github-copilot-vscode-agent" => {
            Agent::GitHubCopilot
        }
        "gitlab-duo" | "gitlab-duo-cli" => Agent::GitLabDuoCli,
        "goose" => Agent::Goose,
        "grok" | "grok-build" => Agent::GrokBuild,
        "hermes" | "hermes-agent" => Agent::Hermes,
        "junie" => Agent::Junie,
        "kilo" | "kilo-code" => Agent::KiloCode,
        "kiro" | "kiro-cli" => Agent::KiroCli,
        "open-hands" | "openhands" => Agent::OpenHands,
        "open-claw" | "openclaw" => Agent::Claw,
        "poolside" | "pool" => Agent::Poolside,
        "neo" | "pulumi-neo" => Agent::PulumiNeo,
        "qwen" | "qwen-code" => Agent::QwenCode,
        "replit" => Agent::Replit,
        "roo-code" => Agent::RooCode,
        "tabnine-cli" => Agent::TabnineCli,
        "trae" => Agent::Trae,
        "v0" => Agent::V0,
        "warp" | "warp-oz" => Agent::Warp,
        _ => return None,
    })
}

/// Normalise an identifier so casing and separator drift still match a
/// canonical name: trim, drop an `@version` suffix, lowercase, and collapse
/// runs of `_`, `-` and whitespace into a single `-`.
fn normalize_agent_value(raw: &str) -> String {
    raw.trim()
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .split(|c: char| c == '_' || c == '-' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A lookup over an explicit map, so a test never reads the environment of
    /// the machine running it (`test-hygiene.md`: env vars are process-global).
    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).map(OsString::from)
    }

    fn detect_in(pairs: &[(&str, &str)]) -> Option<Agent> {
        detect_with(lookup_from(pairs))
    }

    #[test]
    fn an_empty_environment_is_a_human() {
        assert_eq!(detect_in(&[]), None);
    }

    #[test]
    fn every_tool_marker_names_its_own_agent() {
        let cases = [
            (CLAUDECODE, Agent::ClaudeCode),
            (CLAUDE_CODE, Agent::ClaudeCode),
            (CLAUDE_CODE_IS_COWORK, Agent::ClaudeCodeCowork),
            (CURSOR_TRACE_ID, Agent::Cursor),
            (CODEX_SANDBOX, Agent::Codex),
            (CODEX_CI, Agent::Codex),
            (CODEX_THREAD_ID, Agent::Codex),
            (CODEX_SHELL, Agent::Codex),
            (GEMINI_CLI, Agent::GeminiCli),
            (ANTIGRAVITY_AGENT, Agent::Antigravity),
            (COPILOT_AGENT, Agent::GitHubCopilot),
            (JUNIE_SHIM_PATH, Agent::Junie),
            (KILO_PID, Agent::KiloCode),
            (HERMES_SESSION_ID, Agent::Hermes),
            (OPENCODE_CLIENT, Agent::OpenCode),
            (AUGMENT_AGENT, Agent::Augment),
            (REPL_ID, Agent::Replit),
            (DIRAC_ACTIVE, Agent::Dirac),
            (CLINE_ACTIVE, Agent::Cline),
            (ROO_ACTIVE, Agent::RooCode),
            (ROO_CLI_RUNTIME, Agent::RooCode),
            (TRAE_AI_SHELL_ID, Agent::Trae),
            (TABNINE_CLI, Agent::TabnineCli),
            (PI_CODING_AGENT, Agent::Pi),
        ];
        for (var, expected) in cases {
            assert_eq!(
                detect_in(&[(var, "1")]),
                Some(expected),
                "{var} alone must name {expected}"
            );
        }
    }

    #[test]
    fn the_ai_agent_variable_wins_over_every_marker() {
        assert_eq!(
            detect_in(&[(AI_AGENT, "codex"), (CLAUDECODE, "1")]),
            Some(Agent::Codex),
            "AI_AGENT is the cross-harness convention and is read first"
        );
        assert_eq!(
            detect_in(&[(AI_AGENT, "claude-code"), (PI_CODING_AGENT, "1")]),
            Some(Agent::ClaudeCode),
            "a harness that sets AI_AGENT is not overridden by a marker it inherits"
        );
    }

    #[test]
    fn a_fork_marker_beats_the_upstream_it_inherits() {
        assert_eq!(
            detect_in(&[(OPENCODE_CLIENT, "opencode"), (KILO_PID, "42")]),
            Some(Agent::KiloCode),
            "Kilo is an OpenCode fork: its own marker has to win"
        );
    }

    #[test]
    fn the_generic_agent_var_is_read_last_and_only_on_its_allowlist() {
        assert_eq!(detect_in(&[(AGENT, "codex")]), Some(Agent::Codex));
        assert_eq!(detect_in(&[(AGENT, "amp")]), Some(Agent::Amp));
        assert_eq!(detect_in(&[(AGENT, "goose")]), Some(Agent::Goose));
        assert_eq!(detect_in(&[(AGENT, "crush")]), Some(Agent::Crush));
        assert_eq!(
            detect_in(&[(AGENT, "my-shell-helper")]),
            None,
            "a generic name off the allowlist must not read as an agent"
        );
        assert_eq!(
            detect_in(&[(AGENT, "crush"), (CLAUDECODE, "1")]),
            Some(Agent::ClaudeCode),
            "the allowlisted fallback must lose to a tool-specific marker"
        );
    }

    #[test]
    fn an_unrecognized_ai_agent_still_counts_as_an_agent() {
        assert_eq!(
            detect_in(&[(AI_AGENT, "something-new")]),
            Some(Agent::Unknown),
            "setting AI_AGENT is itself the signal that an agent is driving"
        );
        assert_eq!(
            detect_in(&[(AI_AGENT, "")]),
            None,
            "an empty value is no signal"
        );
    }

    #[test]
    fn the_generic_var_normalizes_casing_separators_and_versions() {
        for raw in [
            "Claude_Code",
            "claude code",
            "claude-code@2",
            "  CLAUDE-CODE  ",
        ] {
            assert_eq!(
                detect_in(&[(AI_AGENT, raw)]),
                Some(Agent::ClaudeCode),
                "{raw:?} must normalise to claude-code"
            );
        }
        assert_eq!(
            detect_in(&[(AI_AGENT, "claude-code_2-1-202_agent")]),
            Some(Agent::ClaudeCode),
            "a version decoration without @ still resolves by prefix"
        );
        assert_eq!(
            detect_in(&[(AI_AGENT, "gitlab-lsp_7.17.0__duo-cli")]),
            Some(Agent::GitLabDuoCli)
        );
    }

    #[test]
    fn warp_reports_its_harness_before_its_bare_run_id() {
        assert_eq!(detect_in(&[(OZ_HARNESS, "oz")]), Some(Agent::Warp));
        assert_eq!(
            detect_in(&[(OZ_RUN_ID, "run-1"), (PI_CODING_AGENT, "1")]),
            Some(Agent::Pi),
            "a delegated harness marker outranks an inherited run id"
        );
        assert_eq!(detect_in(&[(OZ_RUN_ID, "run-1")]), Some(Agent::Warp));
    }

    #[test]
    fn cursor_only_claims_its_cli_role_at_the_agent_exec_value() {
        assert_eq!(
            detect_in(&[(CURSOR_EXTENSION_HOST_ROLE, "agent-exec")]),
            Some(Agent::CursorCli)
        );
        assert_eq!(
            detect_in(&[(CURSOR_EXTENSION_HOST_ROLE, "some-other-role")]),
            None,
            "another role is a human using the extension"
        );
        assert_eq!(detect_in(&[(CURSOR_AGENT, "1")]), Some(Agent::CursorCli));
    }

    #[test]
    fn a_value_must_be_set_to_read() {
        assert_eq!(
            detect_in(&[(CLAUDECODE, "")]),
            None,
            "an empty marker is the variable being defined without a run behind it"
        );
    }

    #[test]
    fn parse_accepts_every_id_detection_reports_and_rejects_the_rest() {
        for agent in [
            Agent::ClaudeCode,
            Agent::Codex,
            Agent::Pi,
            Agent::Antigravity,
        ] {
            let id = agent.as_str();
            assert_eq!(
                id.parse::<Agent>().ok(),
                Some(agent),
                "{id} must round-trip"
            );
        }
        assert_eq!("not-an-agent".parse::<Agent>(), Err(ParseAgentError));
        assert_eq!(
            "unknown".parse::<Agent>(),
            Err(ParseAgentError),
            "Unknown is what detection falls back to, never a name a caller may set"
        );
    }

    #[test]
    fn the_published_environment_list_is_the_one_detection_reads() {
        assert_eq!(
            ENVIRONMENT_VARIABLES.len(),
            44,
            "a marker dropped from the table stops detection silently"
        );
        assert!(
            ENVIRONMENT_VARIABLES.contains(&"PI_CODING_AGENT"),
            "the list the docs and tests share must list what detection reads"
        );
        assert_eq!(
            ENVIRONMENT_VARIABLES.len(),
            44,
            "a marker dropped from the table stops detection silently"
        );
    }
}
