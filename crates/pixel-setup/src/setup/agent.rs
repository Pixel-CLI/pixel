// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The agent CLIs `pixel setup` can write a steering block for.
//!
//! One target per harness, the way GitButler's wizard does it, rather than one
//! generic "any agent" target: each harness reads a different instruction file
//! and keeps it in a different place, and a target that cannot name the file it
//! writes cannot be reviewed honestly. A detected agent with no target of its
//! own maps to [`AgentTarget::AgentMd`], which writes the repository's
//! `AGENTS.md` — the file every harness that follows the convention reads.

use std::path::{Path, PathBuf};

use super::detect::Agent;

/// Where a setup applies. The base directory is `$HOME` or the repository
/// root, and it decides both what is detected and what is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The machine: `$HOME` and the harness configuration under it.
    Global,
    /// One repository: its root and the harness configuration inside it.
    Repository,
}

/// One agent CLI the wizard can set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentTarget {
    /// Claude Code.
    ClaudeCode,
    /// OpenAI Codex CLI.
    Codex,
    /// Devin CLI.
    Devin,
    /// Cursor.
    Cursor,
    /// Gemini CLI.
    Gemini,
    /// Antigravity.
    Antigravity,
    /// OpenCode.
    OpenCode,
    /// pi.
    Pi,
    /// Every other agent that reads a repository `AGENTS.md`.
    AgentMd,
}

impl AgentTarget {
    /// Every target, in the order the wizard lists them. The position in this
    /// array is the 1-based index `--selected-agents` and `--help` print, so
    /// reordering it renumbers the flag's answers.
    pub const ALL: [Self; 9] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Devin,
        Self::Cursor,
        Self::Gemini,
        Self::Antigravity,
        Self::OpenCode,
        Self::Pi,
        Self::AgentMd,
    ];

    /// The 1-based index the non-interactive `--selected-agents` flag uses.
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|target| *target == self)
            .expect("every target is listed in ALL")
            + 1
    }

    /// The name the picker row and the review screen print.
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Devin => "Devin",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::Antigravity => "Antigravity",
            Self::OpenCode => "OpenCode",
            Self::Pi => "pi",
            Self::AgentMd => "AGENTS.md",
        }
    }

    /// The row's help text: what installing into this target actually changes.
    pub fn help(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Install the pixel skill under ~/.claude/skills.",
            // The skill is the write; Codex's hooks ride along automatically.
            Self::Codex => "Install the pixel skill under ~/.codex/skills and Codex's hooks.",
            Self::Devin => "Install the pixel skill under ~/.devin/skills.",
            Self::Cursor => "Install the pixel skill under ~/.cursor/skills.",
            Self::Gemini => "Install the pixel skill under ~/.gemini/skills.",
            Self::Antigravity => "Install the pixel skill under the shared ~/.gemini/skills.",
            Self::OpenCode => "Install the pixel skill under ~/.config/opencode/skills.",
            Self::Pi => "Install the pixel skill and the impact/brief package under ~/.pi/agent.",
            Self::AgentMd => "No harness directory of its own; nothing is written for it.",
        }
    }

    /// The setup target a detected agent belongs to. Every agent pixel does
    /// not have a target of its own maps to [`AgentTarget::AgentMd`], so a
    /// harness we cannot name still lands in the picker.
    pub fn from_detected(agent: Agent) -> Self {
        match agent {
            Agent::ClaudeCode | Agent::ClaudeCodeCowork => Self::ClaudeCode,
            Agent::Codex => Self::Codex,
            Agent::Devin | Agent::DevinCli => Self::Devin,
            Agent::Cursor | Agent::CursorCli => Self::Cursor,
            Agent::GeminiCli => Self::Gemini,
            Agent::Antigravity | Agent::AntigravityCli => Self::Antigravity,
            Agent::OpenCode => Self::OpenCode,
            Agent::Pi => Self::Pi,
            Agent::Unknown
            | Agent::Amp
            | Agent::AmazonQ
            | Agent::Augment
            | Agent::Claw
            | Agent::Cline
            | Agent::CodeBuddy
            | Agent::Crush
            | Agent::DeepSeekHarness
            | Agent::Dirac
            | Agent::GitHubCopilot
            | Agent::GitLabDuoCli
            | Agent::Goose
            | Agent::GrokBuild
            | Agent::Hermes
            | Agent::Junie
            | Agent::KiloCode
            | Agent::KiroCli
            | Agent::OpenHands
            | Agent::Poolside
            | Agent::PulumiNeo
            | Agent::QwenCode
            | Agent::Replit
            | Agent::RooCode
            | Agent::TabnineCli
            | Agent::Trae
            | Agent::V0
            | Agent::Warp => Self::AgentMd,
        }
    }

    /// The configuration directory (or file) whose presence means this agent is
    /// set up for this user, relative to `$HOME`. `None` when the agent has no
    /// configuration of its own to look for — the generic target shares one
    /// file with seven others, so it is never evidence for anything.
    pub fn home_marker(self) -> Option<&'static [&'static str]> {
        Some(match self {
            Self::ClaudeCode => &[".claude"],
            Self::Codex => &[".codex"],
            Self::Devin => &[".config", "devin"],
            Self::Cursor => &[".cursor"],
            Self::Gemini => &[".gemini", "settings.json"],
            Self::Antigravity => &[".gemini", "config"],
            Self::OpenCode => &[".config", "opencode"],
            Self::Pi => &[".pi", "agent"],
            Self::AgentMd => return None,
        })
    }

    /// An unambiguous per-repository marker for this agent.
    ///
    /// Deliberately never `AGENTS.md` and never `.gemini`: seven targets read
    /// the first and two share the second, so neither is evidence about one
    /// agent. `None` when the repository carries nothing this agent owns.
    pub fn repo_marker(self) -> Option<&'static [&'static str]> {
        Some(match self {
            Self::ClaudeCode => &["CLAUDE.md"],
            Self::Codex => &[".codex"],
            Self::Devin => &[".devin"],
            Self::Cursor => &[".cursor"],
            Self::Gemini | Self::Antigravity => &[".gemini"],
            Self::OpenCode => &[".config", "opencode"],
            Self::Pi => &[".pi"],
            Self::AgentMd => return None,
        })
    }

    /// Whether this agent looks like it is already in use under `base` for
    /// `scope`, so the picker can pre-check it. The wizard checks the running
    /// agent separately: a harness with no configuration directory on this
    /// machine can still be the one asking.
    pub fn in_use(self, base: &Path, scope: Scope) -> bool {
        let marker = match scope {
            Scope::Global => self.home_marker(),
            Scope::Repository => self.repo_marker(),
        };
        marker.is_some_and(|components| join(base, components).exists())
    }
}

/// Join path components onto a base directory, platform-native separators.
pub(crate) fn join(base: &Path, components: &[&str]) -> PathBuf {
    components
        .iter()
        .fold(base.to_path_buf(), |path, component| path.join(component))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_has_a_distinct_index_and_name() {
        let indices: Vec<usize> = AgentTarget::ALL.iter().map(|t| t.index()).collect();
        assert_eq!(indices, (1..=9).collect::<Vec<usize>>());
        let names: Vec<&str> = AgentTarget::ALL.iter().map(|t| t.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "two targets share a name");
    }

    #[test]
    fn the_agents_pixel_names_map_to_their_own_target() {
        let cases = [
            (Agent::ClaudeCode, AgentTarget::ClaudeCode),
            (Agent::ClaudeCodeCowork, AgentTarget::ClaudeCode),
            (Agent::Codex, AgentTarget::Codex),
            (Agent::Devin, AgentTarget::Devin),
            (Agent::DevinCli, AgentTarget::Devin),
            (Agent::Cursor, AgentTarget::Cursor),
            (Agent::CursorCli, AgentTarget::Cursor),
            (Agent::GeminiCli, AgentTarget::Gemini),
            (Agent::Antigravity, AgentTarget::Antigravity),
            (Agent::AntigravityCli, AgentTarget::Antigravity),
            (Agent::OpenCode, AgentTarget::OpenCode),
            (Agent::Pi, AgentTarget::Pi),
        ];
        for (agent, expected) in cases {
            assert_eq!(AgentTarget::from_detected(agent), expected, "{agent}");
        }
    }

    #[test]
    fn an_agent_pixel_cannot_name_still_gets_the_generic_target() {
        for agent in [
            Agent::Unknown,
            Agent::Amp,
            Agent::Cline,
            Agent::Warp,
            Agent::Crush,
        ] {
            assert_eq!(
                AgentTarget::from_detected(agent),
                AgentTarget::AgentMd,
                "{agent} must not silently lose its steering file"
            );
        }
    }

    #[test]
    fn the_agents_md_file_is_never_a_detection_marker() {
        assert_eq!(AgentTarget::AgentMd.home_marker(), None);
        assert_eq!(AgentTarget::AgentMd.repo_marker(), None);
        for target in AgentTarget::ALL {
            assert_ne!(
                target.repo_marker(),
                Some(&["AGENTS.md"][..]),
                "{} must not claim the shared file as its own",
                target.name()
            );
        }
    }

    #[test]
    fn in_use_reads_the_marker_of_the_scope_it_is_given() {
        let machine = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        assert!(
            !AgentTarget::Codex.in_use(machine.path(), Scope::Global),
            "an empty home has no Codex configuration"
        );
        std::fs::create_dir_all(machine.path().join(".codex")).unwrap();
        std::fs::create_dir_all(repo.path().join(".codex")).unwrap();
        std::fs::write(repo.path().join("CLAUDE.md"), "# rules\n").unwrap();
        std::fs::create_dir_all(machine.path().join(".cursor")).unwrap();

        assert!(AgentTarget::Codex.in_use(machine.path(), Scope::Global));
        assert!(AgentTarget::Codex.in_use(repo.path(), Scope::Repository));
        assert!(
            !AgentTarget::Cursor.in_use(repo.path(), Scope::Repository),
            "a repository with no .cursor is not evidence Claude is missing there"
        );
        assert!(
            !AgentTarget::ClaudeCode.in_use(machine.path(), Scope::Global),
            "a repository CLAUDE.md says nothing about this machine"
        );
        assert!(
            AgentTarget::ClaudeCode.in_use(repo.path(), Scope::Repository),
            "a repository CLAUDE.md is the one unambiguous per-repository marker"
        );
        assert!(!AgentTarget::AgentMd.in_use(machine.path(), Scope::Global));
    }
}
