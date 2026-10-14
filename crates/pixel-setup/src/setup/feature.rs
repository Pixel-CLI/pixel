// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The pixel features the setup wizard offers, and what each one writes.
//!
//! GitButler's wizard asks which *workflow policies* it should encode as
//! instructions ("amend small fixes into the matching commit"). Pixel already
//! ships those behaviours as features, so the same question is worth asking of
//! them: metrics, the per-prompt brief, the daemon, classification.
//!
//! A feature is a bundle of up to two things, and saying which is the point:
//!
//! - [`Writes::Config`] — a key in the global `~/.pixel/config.yaml`;
//! - [`Writes::Hooks`] — a group of hook registrations.
//!
//! `pixel setup` owns the hook files and the `pixel` skill. The review prints
//! each install step (from a dry run of the same code that applies it), so
//! nothing is written that the confirmation did not show.

/// Where a feature's effect lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// A key in the global configuration, as a path below its root
    /// (`["classify", "enabled"]`) and the value to store.
    Config(&'static [&'static str], ConfigValue),
    /// A group of hook registrations, applied by the installer for the
    /// selected agents rather than rendered by this crate.
    Hooks(HookGroup),
}

/// A group of hook registrations a feature switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookGroup {
    /// The task-event hooks of Claude Code, Devin, Gemini CLI, Antigravity and
    /// OpenCode.
    Task,
    /// Codex's task hooks under `~/.codex`.
    Codex,
    /// The pi impact extension under `~/.pi/agent`.
    Pi,
}

/// A value the wizard stores in the global configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValue {
    /// `true` / `false`.
    Bool(bool),
    /// A quoted string, written as YAML text (`metrics: "on"`).
    Text(&'static str),
}

impl ConfigValue {
    /// The `serde_json::Value` the configuration document holds for it. The
    /// global file is YAML parsed into JSON values, so a string stays a string
    /// and a bare `off` never becomes a boolean.
    pub fn to_json(self) -> serde_json::Value {
        match self {
            Self::Bool(value) => serde_json::Value::Bool(value),
            Self::Text(value) => serde_json::Value::String(value.to_string()),
        }
    }
}

/// A pixel feature the wizard can activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    /// Token super savior: retrieval-first workflow, prompt-submit brief hook
    /// and per-prompt brief config in one feature.
    Prompt,
    /// Command timing and savings.
    Metrics,
    /// Background daemon on demand.
    Daemon,
    /// AI classification.
    Classify,
}

impl Feature {
    /// Every feature, in the order the wizard lists them. The position is the
    /// 1-based index `--selected-features` and `--help` print.
    pub const ALL: [Self; 4] = [Self::Prompt, Self::Metrics, Self::Daemon, Self::Classify];

    /// The 1-based index the non-interactive `--selected-features` flag uses.
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|feature| *feature == self)
            .expect("every feature is listed in ALL")
            + 1
    }

    /// The stable identifier, as printed in JSON output and the action log.
    pub fn id(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Metrics => "metrics",
            Self::Daemon => "daemon",
            Self::Classify => "classify",
        }
    }

    /// The picker's row label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Prompt => "Token super savior",
            Self::Metrics => "Show command timing and savings",
            Self::Daemon => "Start the background daemon on demand",
            Self::Classify => "AI classification",
        }
    }

    /// The row's help text.
    pub fn help(self) -> &'static str {
        match self {
            Self::Prompt => {
                "Teach the agent to retrieve with pixel before reading files, keep edits inside the scoped task, register the user prompt-submit hook, and generate a per-prompt brief (pixel config: brief)."
            }
            Self::Metrics => {
                "Show timing and estimated savings after each command (pixel config: metrics)."
            }
            Self::Daemon => {
                "Start a background process on the first query of a session, so later ones are fast."
            }
            Self::Classify => {
                "Let an AI decision engine answer the agent's questions about files. Setup then asks which engine: local Ollaya (offline, $0), Cloudflare Clef-flash (Ollama or Workers AI), Jev, OpenAI Decisions, or a remote LLM preset behind your key; `pixel config classify-engine` switches later."
            }
        }
    }

    /// What this feature writes. A feature never writes two things of different
    /// kinds at once; a feature that both steers and configures would be two
    /// features.
    pub fn writes(self) -> Writes {
        match self {
            Self::Prompt => Writes::Hooks(HookGroup::Task),
            Self::Metrics => Writes::Config(&["metrics"], ConfigValue::Text("on")),
            Self::Daemon => Writes::Config(&["daemon_auto_start"], ConfigValue::Bool(true)),
            Self::Classify => Writes::Config(&["classify", "enabled"], ConfigValue::Bool(true)),
        }
    }

    /// Whether the wizard pre-checks this feature. The defaults are the ones
    /// that cost nothing and need no credential. Anything that spends money,
    /// needs a key, or starts a background service is opt-in.
    pub fn default_selected(self) -> bool {
        matches!(self, Self::Prompt | Self::Metrics | Self::Daemon)
    }

    /// Whether this preference only makes sense for one repository.
    ///
    /// The steering is rendered once and written to every file the setup
    /// targets, so a repository-local rule would also land in the user's global
    /// instruction files. No feature in the current catalog is repo-local.
    pub fn repo_local_only(self) -> bool {
        false
    }

    /// The help shown for a repository-local feature under any other scope: it
    /// has to say how to get it, not merely that it is unavailable.
    pub fn repo_local_help(self) -> &'static str {
        "Re-run this setup with --repo <path> for a single repository to land work directly on the target branch instead of opening a pull request."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_has_a_distinct_index_and_id() {
        let indices: Vec<usize> = Feature::ALL.iter().map(|f| f.index()).collect();
        assert_eq!(indices, (1..=Feature::ALL.len()).collect::<Vec<usize>>());
        let ids: Vec<&str> = Feature::ALL.iter().map(|f| f.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "two features share an id");
    }

    #[test]
    fn every_feature_says_what_it_writes_and_carries_its_own_help() {
        for feature in Feature::ALL {
            let _ = feature.writes();
            assert!(!feature.label().is_empty(), "{} has no label", feature.id());
            assert!(
                feature.help().len() > 30,
                "{} needs help a reader can act on",
                feature.id()
            );
        }
    }

    #[test]
    fn the_free_features_and_the_hook_groups_are_pre_checked() {
        let selected: Vec<&str> = Feature::ALL
            .iter()
            .filter(|f| f.default_selected())
            .map(|f| f.id())
            .collect();
        assert_eq!(selected, vec!["prompt", "metrics", "daemon"]);
    }

    #[test]
    fn no_feature_is_repository_local_in_the_current_catalog() {
        let local: Vec<&str> = Feature::ALL
            .iter()
            .filter(|f| f.repo_local_only())
            .map(|f| f.id())
            .collect();
        assert_eq!(local, Vec::<&str>::new());
    }

    #[test]
    fn a_config_feature_names_a_key_that_exists_in_the_template() {
        let keys: Vec<&[&str]> = Feature::ALL
            .iter()
            .filter_map(|f| match f.writes() {
                Writes::Config(key, _) => Some(key),
                Writes::Hooks(_) => None,
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                &["metrics"][..],
                &["daemon_auto_start"][..],
                &["classify", "enabled"][..],
            ]
        );
    }

    #[test]
    fn the_prompt_feature_installs_the_task_hooks() {
        assert_eq!(
            Feature::Prompt.writes(),
            Writes::Hooks(HookGroup::Task),
            "the prompt-submit hook is what Token super savior writes"
        );
    }

    #[test]
    fn metrics_stays_a_quoted_string() {
        assert_eq!(
            Feature::Metrics.writes(),
            Writes::Config(&["metrics"], ConfigValue::Text("on"))
        );
        assert_eq!(
            ConfigValue::Text("on").to_json(),
            serde_json::Value::String("on".into()),
            "a bare off would parse as a YAML boolean"
        );
    }
}
