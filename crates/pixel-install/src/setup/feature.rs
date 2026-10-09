// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The pixel features the setup wizard offers, and what each one writes.
//!
//! GitButler's wizard asks which *workflow policies* it should encode as
//! instructions ("amend small fixes into the matching commit"). Pixel already
//! ships those behaviours as features, so the same question is worth asking of
//! them: metrics, the per-prompt brief, the daemon, classification, search.
//!
//! A feature is a bundle of up to three things, and saying which is the point:
//!
//! - [`Writes::Steering`] — a section in the managed block of every selected
//!   agent's instruction file;
//! - [`Writes::Config`] — a key in the global `~/.pixel/config.yaml`;
//! - [`Writes::Rules`] — a per-agent rule file, for the harnesses that load one;
//! - [`Writes::Note`] — nothing to write, because `pixel install` or
//!   `pixel config setup` already owns that artifact and the setup says so
//!   rather than writing a second copy of it.
//!
//! The wizard never writes a hook entry itself. `pixel install` owns the hook
//! files, and a second writer of one settings file is how a user's own hooks
//! get lost.

/// Where a feature's effect lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// A `###` section inside the managed block of every selected agent's
    /// instruction file.
    Steering(&'static str),
    /// A key in the global configuration, as a path below its root
    /// (`["classify", "enabled"]`) and the value to store.
    Config(&'static [&'static str], ConfigValue),
    /// A per-agent rule file under the harness's rules directory, when it has
    /// one: the same steering, in the file that harness loads per tool.
    Rules,
    /// Nothing to write: another command owns it, and the wizard names it.
    Note(&'static str),
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
    /// Retrieval-first workflow.
    Prompt,
    /// Per-prompt brief.
    Brief,
    /// Guard destructive git commands.
    Guard,
    /// Command timing and savings.
    Metrics,
    /// Background daemon on demand.
    Daemon,
    /// Semantic search first.
    Semantic,
    /// AI classification.
    Classify,
    /// Private web-search provider.
    WebSearch,
    /// Per-agent rule files.
    Rules,
    /// The pi impact extension.
    PiExtension,
    /// Codex config and hooks.
    CodexConfig,
    /// Land on the target branch.
    Land,
}

impl Feature {
    /// Every feature, in the order the wizard lists them. The position is the
    /// 1-based index `--selected-features` and `--help` print.
    pub const ALL: [Self; 12] = [
        Self::Prompt,
        Self::Brief,
        Self::Guard,
        Self::Metrics,
        Self::Daemon,
        Self::Semantic,
        Self::Classify,
        Self::WebSearch,
        Self::Rules,
        Self::PiExtension,
        Self::CodexConfig,
        Self::Land,
    ];

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
            Self::Brief => "brief",
            Self::Guard => "guard",
            Self::Metrics => "metrics",
            Self::Daemon => "daemon",
            Self::Semantic => "semantic",
            Self::Classify => "classify",
            Self::WebSearch => "web-search",
            Self::Rules => "rules",
            Self::PiExtension => "pi-extension",
            Self::CodexConfig => "codex-config",
            Self::Land => "land",
        }
    }

    /// The picker's row label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Prompt => "Retrieval-first workflow",
            Self::Brief => "Answer each prompt with a scoped brief",
            Self::Guard => "Guard destructive git commands",
            Self::Metrics => "Show command timing and savings",
            Self::Daemon => "Start the background daemon on demand",
            Self::Semantic => "Prefer semantic search",
            Self::Classify => "AI classification",
            Self::WebSearch => "Private web-search provider",
            Self::Rules => "Per-agent rule files",
            Self::PiExtension => "pi impact extension",
            Self::CodexConfig => "Codex config and hooks",
            Self::Land => "Land on the target branch",
        }
    }

    /// The row's help text.
    pub fn help(self) -> &'static str {
        match self {
            Self::Prompt => {
                "Teach the agent to retrieve with pixel before reading files, and to keep its own edits inside the scoped task."
            }
            Self::Brief => {
                "Print a bounded evidence brief for each project-related prompt (pixel config: brief)."
            }
            Self::Guard => {
                "Name the hook that blocks a destructive git command. `pixel install` writes it; the setup never edits a settings file."
            }
            Self::Metrics => {
                "Show timing and estimated savings after each command (pixel config: metrics)."
            }
            Self::Daemon => {
                "Start a background process on the first query of a session, so later ones are fast."
            }
            Self::Semantic => {
                "Prefer `pixel search-meaning` for a question about intent, and `pixel search-content` when you already have the text."
            }
            Self::Classify => {
                "Classify files with an AI engine. `pixel config setup` then asks for the engine and its credentials."
            }
            Self::WebSearch => {
                "Use a private provider for `pixel web-search` instead of the public chain. `pixel config setup` asks for it."
            }
            Self::Rules => {
                "Also write the steering into the per-tool rule file the harness loads (rules/ directory)."
            }
            Self::PiExtension => {
                "The pi impact extension `pixel install` deploys under ~/.pi/agent."
            }
            Self::CodexConfig => {
                "The codex config and hooks `pixel install` writes under ~/.codex."
            }
            Self::Land => {
                "Publish work by landing on the target branch instead of opening a pull request. Single repository only: the rule must not reach your global configuration."
            }
        }
    }

    /// What this feature writes. A feature never writes two things of different
    /// kinds at once; a feature that both steers and configures would be two
    /// features.
    pub fn writes(self) -> Writes {
        match self {
            Self::Prompt => Writes::Steering(PROMPT_STEERING),
            Self::Brief => Writes::Config(&["brief"], ConfigValue::Bool(true)),
            Self::Guard => Writes::Note(
                "the guard hook is written by `pixel install`; this setup does not edit hook settings",
            ),
            Self::Metrics => Writes::Config(&["metrics"], ConfigValue::Text("on")),
            Self::Daemon => Writes::Config(&["daemon_auto_start"], ConfigValue::Bool(true)),
            Self::Semantic => Writes::Steering(SEMANTIC_STEERING),
            Self::Classify => Writes::Config(&["classify", "enabled"], ConfigValue::Bool(true)),
            Self::WebSearch => Writes::Steering(WEB_SEARCH_STEERING),
            Self::Rules => Writes::Rules,
            Self::PiExtension => Writes::Note(
                "the pi impact extension is written by `pixel install` under ~/.pi/agent",
            ),
            Self::CodexConfig => Writes::Note(
                "the codex config and hooks are written by `pixel install` under ~/.codex",
            ),
            Self::Land => Writes::Steering(LAND_STEERING),
        }
    }

    /// Whether the wizard pre-checks this feature. The five defaults are the
    /// ones that cost nothing, need no credential and change no git history;
    /// anything that spends money, needs a key, or changes how a branch is
    /// published is opt-in.
    pub fn default_selected(self) -> bool {
        matches!(
            self,
            Self::Prompt | Self::Brief | Self::Guard | Self::Metrics | Self::Daemon
        )
    }

    /// Whether this preference only makes sense for one repository.
    ///
    /// The steering is rendered once and written to every file the setup
    /// targets, so a repository-local rule (landing on a branch instead of
    /// opening a pull request) must not be offered for a machine-wide setup,
    /// where it would also land in the user's global instruction files.
    pub fn repo_local_only(self) -> bool {
        matches!(self, Self::Land)
    }

    /// The help shown for a repository-local feature under any other scope: it
    /// has to say how to get it, not merely that it is unavailable.
    pub fn repo_local_help(self) -> &'static str {
        "Re-run this setup with --repo <path> for a single repository to land work directly on the target branch instead of opening a pull request."
    }
}

/// The baseline section: what the agent is told to do, and what it is told
/// pixel already refuses to do.
pub(crate) const PROMPT_STEERING: &str = "\
- Use pixel for repository retrieval: `pixel scope-task` for the file list, `pixel search-content` for a literal search, `pixel search-meaning` for a question about intent, `pixel impact` and `pixel who-wrote` before editing a symbol.
- Read a file only when the retrieval result does not answer the question; do not sweep the tree first.
- Keep every edit inside the scoped task. A file outside it needs the user to say so.
- Commit messages and pull request descriptions say what changed and why, in a few lines.
- Pixel does not manage pull requests, issues or reviews. Use the host's own tooling for those.";

/// The semantic-search steering.
pub(crate) const SEMANTIC_STEERING: &str = "\
- When the question is about intent, concepts or behaviour, run `pixel search-meaning` before reading files.
- When the literal text is known, `pixel search-content` is cheaper and more precise.
- Do not run both for the same question: pick the one that matches what is being asked.";

/// The web-search steering.
pub(crate) const WEB_SEARCH_STEERING: &str = "\
- `pixel web-search` answers a question that is not in this repository. Without a configured provider it falls back to the public chain, so do not send private context to it.
- Prefer the repository: search the code before the web.";

/// The repository-local landing steering.
pub(crate) const LAND_STEERING: &str = "\
- This repository lands work directly on its target branch: there is no pull request here.
- When the user approves the work as finished, commit it on the session's own branch and land it on the target with the host's own branch-protection flow. Ask before landing.
- This rule is about this repository. A pull-request workflow in another repository is not contradicted by it.";

/// The section title a steering feature renders under. Every other feature
/// writes its effect somewhere other than the block, so asking it for a title
/// is the caller's bug rather than a missing case to paper over.
pub(crate) fn steering_title(feature: Feature) -> &'static str {
    match feature {
        Feature::Prompt => "Pixel",
        Feature::Semantic => "Search",
        Feature::WebSearch => "Web search",
        Feature::Land => "Publishing",
        Feature::Brief
        | Feature::Guard
        | Feature::Metrics
        | Feature::Daemon
        | Feature::Classify
        | Feature::Rules
        | Feature::PiExtension
        | Feature::CodexConfig => {
            panic!("{} writes no steering section", feature.id())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_has_a_distinct_index_and_id() {
        let indices: Vec<usize> = Feature::ALL.iter().map(|f| f.index()).collect();
        assert_eq!(indices, (1..=12).collect::<Vec<usize>>());
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
    fn only_the_five_free_features_are_pre_checked() {
        let selected: Vec<&str> = Feature::ALL
            .iter()
            .filter(|f| f.default_selected())
            .map(|f| f.id())
            .collect();
        assert_eq!(
            selected,
            vec!["prompt", "brief", "guard", "metrics", "daemon"]
        );
    }

    #[test]
    fn landing_is_the_only_repository_local_feature() {
        let local: Vec<&str> = Feature::ALL
            .iter()
            .filter(|f| f.repo_local_only())
            .map(|f| f.id())
            .collect();
        assert_eq!(local, vec!["land"]);
        assert!(
            !Feature::Land.help().is_empty() && !Feature::Land.repo_local_help().is_empty(),
            "a disabled row still has to explain how to enable it"
        );
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
                &["brief"][..],
                &["metrics"][..],
                &["daemon_auto_start"][..],
                &["classify", "enabled"][..],
            ]
        );
    }

    #[test]
    fn a_steering_feature_has_text_and_a_title() {
        for feature in Feature::ALL {
            if let Writes::Steering(body) = feature.writes() {
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
    fn a_note_names_the_command_that_owns_the_artifact() {
        for feature in Feature::ALL {
            if let Writes::Note(note) = feature.writes() {
                assert!(
                    note.contains("pixel install"),
                    "{} must name the command that writes it",
                    feature.id()
                );
            }
        }
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
