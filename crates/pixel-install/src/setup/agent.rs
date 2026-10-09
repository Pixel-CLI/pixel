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

    /// The row's help text: what writing this target's file actually changes.
    pub fn help(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Write the pixel steering block into CLAUDE.md.",
            Self::Codex => {
                "Write the pixel steering block into AGENTS.md, and the codex config next to it."
            }
            Self::Devin => {
                "Write the pixel steering block into the repository AGENTS.md Devin reads."
            }
            Self::Cursor => {
                "Write the pixel steering block into the repository AGENTS.md Cursor reads."
            }
            Self::Gemini => "Write the pixel steering block into GEMINI.md.",
            Self::Antigravity => "Write the pixel steering block into the repository AGENTS.md.",
            Self::OpenCode => "Write the pixel steering block into the opencode AGENTS.md.",
            Self::Pi => "Write the pixel steering block into the pi system-prompt file.",
            Self::AgentMd => {
                "Write the pixel steering block into the repository AGENTS.md every other agent reads."
            }
        }
    }

    /// The setup target a detected agent belongs to. Every agent pixel does
    /// not have a target of its own maps to [`AgentTarget::AgentMd`], so a
    /// harness we cannot name still gets its steering file.
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

    /// The instruction file this target writes, relative to the scope's base
    /// directory, or `None` when the agent has no file pixel is willing to
    /// guess at. A guessed path is a file the harness never reads, which reads
    /// on the review screen as pixel having done something.
    pub fn instruction(self, scope: Scope) -> Option<&'static [&'static str]> {
        match (self, scope) {
            (Self::ClaudeCode, Scope::Global) => Some(&[".claude", "CLAUDE.md"]),
            (Self::Codex, Scope::Global) => Some(&[".codex", "AGENTS.md"]),
            (Self::OpenCode, Scope::Global) => Some(&[".config", "opencode", "AGENTS.md"]),
            (Self::Pi, Scope::Global) => Some(&[".pi", "agent", "APPEND_SYSTEM.md"]),
            (Self::Gemini, Scope::Global) => Some(&[".gemini", "GEMINI.md"]),
            (_, Scope::Repository) => Some(match self {
                Self::ClaudeCode => &["CLAUDE.md"][..],
                Self::Gemini => &["GEMINI.md"][..],
                _ => &["AGENTS.md"][..],
            }),
            // Cursor's global rules need `.mdc` frontmatter to load at all, the
            // generic target has no global file of its own, and Devin and
            // Antigravity read the project's AGENTS.md only. None of them has a
            // global instruction file pixel writes without guessing.
            (Self::Cursor | Self::Devin | Self::Antigravity | Self::AgentMd, Scope::Global) => None,
        }
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

    /// The absolute path this target's instruction file has under `base`.
    pub fn instruction_path(self, base: &Path, scope: Scope) -> Option<PathBuf> {
        self.instruction(scope)
            .map(|components| join(base, components))
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
    fn every_target_names_a_repository_instruction_file() {
        for target in AgentTarget::ALL {
            assert!(
                target.instruction(Scope::Repository).is_some(),
                "{} writes no repository file",
                target.name()
            );
        }
    }

    #[test]
    fn the_global_targets_that_have_a_file_name_one() {
        let named: Vec<&str> = AgentTarget::ALL
            .iter()
            .filter(|t| t.instruction(Scope::Global).is_some())
            .map(|t| t.name())
            .collect();
        assert_eq!(
            named,
            vec!["Claude Code", "Codex", "Gemini CLI", "OpenCode", "pi"],
            "a global instruction file is claimed only where the harness reads one"
        );
    }

    #[test]
    fn instruction_paths_resolve_under_the_scope_base() {
        let dir = Path::new("/tmp/pixel-setup-agent-test");
        assert_eq!(
            AgentTarget::ClaudeCode.instruction_path(dir, Scope::Repository),
            Some(dir.join("CLAUDE.md"))
        );
        assert_eq!(
            AgentTarget::ClaudeCode.instruction_path(dir, Scope::Global),
            Some(dir.join(".claude").join("CLAUDE.md"))
        );
        assert_eq!(
            AgentTarget::Cursor.instruction_path(dir, Scope::Global),
            None
        );
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
