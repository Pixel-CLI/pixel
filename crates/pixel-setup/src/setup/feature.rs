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
//! - [`Writes::Steering`] — a section in the managed block of every selected
//!   agent's instruction file;
//! - [`Writes::Config`] — a key in the global `~/.pixel/config.yaml`.
//!
//! `pixel setup` owns the hook files. The review prints each hook step (from a
//! dry run of the same code that applies it) next to the instruction files, so
//! nothing is written that the confirmation did not show.

use pixel_plugin::Available;

/// Where a feature's effect lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// A `###` section inside the managed block of every selected agent's
    /// instruction file. Its text is [`Feature::steering`].
    Steering,
    /// A key in the global configuration, as a path below its root
    /// (`["classify", "enabled"]`) and the value to store.
    Config(&'static [&'static str], ConfigValue),
    /// A group of hook registrations, applied by the installer for the
    /// selected agents rather than rendered by this crate. Retired from the
    /// catalog, but the installer still uses [`HookGroup`] directly.
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
            Self::Prompt => Writes::Steering,
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

/// The baseline section up to the commands that follow `pixel impact`: what
/// the agent is told to do, and what it is told pixel already refuses to do.
const PROMPT_RETRIEVAL: &str = "\
- Use pixel for repository retrieval: `pixel scope-task` for the file list, `pixel search-content` for a literal search, `pixel search-meaning` for a question about intent, `pixel impact`";

/// What the retrieval bullet adds when the git plugin runs here.
const PROMPT_WHO_WROTE: &str = " and `pixel who-wrote`";

/// The rest of the baseline, which names no plugin command.
const PROMPT_REST: &str = " before editing a symbol.
- Read a file only when the retrieval result does not answer the question; do not sweep the tree first.
- Keep every edit inside the scoped task. A file outside it needs the user to say so.
- Commit messages and pull request descriptions say what changed and why, in a few lines.
- Pixel does not manage pull requests, issues or reviews. Use the host's own tooling for those.";

/// The steering text of a feature that writes any, for the plugins in
/// `plugins`; `None` for a feature that writes no steering. An agent copies the
/// commands it is given, so a command no plugin answers is left out rather than
/// taught.
impl Feature {
    pub(crate) fn steering(self, plugins: &Available) -> Option<String> {
        if self.writes() != Writes::Steering {
            return None;
        }
        Some(match self {
            Self::Prompt => {
                let who_wrote = if plugins.has("who-wrote") {
                    PROMPT_WHO_WROTE
                } else {
                    ""
                };
                format!("{PROMPT_RETRIEVAL}{who_wrote}{PROMPT_REST}")
            }
            Self::Metrics | Self::Daemon | Self::Classify => {
                unreachable!("{} writes no steering", self.id())
            }
        })
    }
}

/// The section title a steering feature renders under. Every other feature
/// writes its effect somewhere other than the block, so asking it for a title
/// is the caller's bug rather than a missing case to paper over.
pub(crate) fn steering_title(feature: Feature) -> &'static str {
    match feature {
        Feature::Prompt => "Pixel",
        Feature::Metrics | Feature::Daemon | Feature::Classify => {
            panic!("{} writes no steering section", feature.id())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_plugin() -> Available {
        Available::of(&["who-wrote", "web-search"])
    }

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
                _ => None,
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
    fn a_steering_feature_has_text_and_a_title() {
        for feature in Feature::ALL {
            if feature.writes() == Writes::Steering {
                let body = feature
                    .steering(&every_plugin())
                    .expect("a steering feature has text when its plugin runs");
                assert!(
                    body.lines().all(|line| line.starts_with("- ")),
                    "{} renders bullets, not prose",
                    feature.id()
                );
                assert!(!steering_title(feature).is_empty());
            }
        }
    }

    #[test]
    fn no_feature_is_a_hook_group_in_the_current_catalog() {
        let groups: Vec<(&str, HookGroup)> = Feature::ALL
            .iter()
            .filter_map(|f| match f.writes() {
                Writes::Hooks(group) => Some((f.id(), group)),
                _ => None,
            })
            .collect();
        assert_eq!(groups, Vec::<(&str, HookGroup)>::new());
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

    #[test]
    fn the_baseline_names_who_wrote_only_where_the_git_plugin_runs() {
        let without = Feature::Prompt.steering(&Available::none()).unwrap();
        let with = Feature::Prompt.steering(&every_plugin()).unwrap();
        assert!(!without.contains("who-wrote"), "{without}");
        assert!(
            without.contains("`pixel impact` before editing a symbol."),
            "{without}"
        );
        assert!(
            with.contains("`pixel impact` and `pixel who-wrote` before editing a symbol."),
            "{with}"
        );
        // Everything else is the same bullet for bullet.
        assert_eq!(without.lines().count(), with.lines().count());
        assert_eq!(
            without.lines().skip(1).collect::<Vec<_>>(),
            with.lines().skip(1).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_feature_without_steering_has_none_whatever_plugins_run() {
        for feature in Feature::ALL {
            if feature.writes() != Writes::Steering {
                assert_eq!(feature.steering(&every_plugin()), None, "{}", feature.id());
            }
        }
    }
}
