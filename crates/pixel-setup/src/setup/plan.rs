// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What a setup will write, decided before anything is written.
//!
//! The split is the point: a wizard that prompts, writes and reports at the
//! same time cannot show the user what it is about to do. `SetupPlan` resolves
//! every answer to concrete writes first, so the review screen prints them
//! and cancelling writes nothing. The instruction-file block is retired —
//! the `pixel` skill carries the retrieval workflow into every harness's
//! skills dir, and the installer (not this plan) writes it, alongside the
//! hooks. What the plan still owns: the selected agents and features in
//! catalog order, the configuration keys to store, and the notes a user has
//! to act on by hand.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::install::InstallSelection;

use super::agent::{AgentTarget, Scope};
use super::feature::{ConfigValue, Feature, HookGroup, Writes};

/// A key the selected features store in the global configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWrite {
    /// The key below the configuration root, e.g. `["classify", "enabled"]`.
    pub key: &'static [&'static str],
    /// The value to store.
    pub value: ConfigValue,
}

/// Every write a setup performs, resolved from the answers.
#[derive(Debug, Clone)]
pub struct SetupPlan {
    /// The directory the paths are relative to: `$HOME`, the repository root,
    /// or the dummy root under `--dummy-apply`.
    pub base: PathBuf,
    /// Whether the base is the machine or one repository.
    pub scope: Scope,
    /// The agents selected, in catalog order.
    pub agents: Vec<AgentTarget>,
    /// The features selected, in catalog order.
    pub features: Vec<Feature>,
    /// The configuration keys to store, applied by the caller: this crate does
    /// not own the global YAML document.
    pub config_writes: Vec<ConfigWrite>,
    /// What the user has to do by hand.
    pub notes: Vec<String>,
}

impl SetupPlan {
    /// Resolve the answers into a plan. `base` is the scope's root directory:
    /// nothing is read from or written to it here.
    ///
    /// Features that only make sense for one repository are dropped outside a
    /// repository scope rather than refused: the wizard offers them only there,
    /// so a non-interactive run that names one under `--repo`-less scope is a
    /// stale answer, not a reason to fail the whole setup.
    pub fn new(
        base: &Path,
        scope: Scope,
        agents: &[AgentTarget],
        features: &[Feature],
    ) -> Result<Self> {
        let agents: Vec<AgentTarget> = AgentTarget::ALL
            .iter()
            .copied()
            .filter(|agent| agents.contains(agent))
            .collect();
        let features: Vec<Feature> = Feature::ALL
            .iter()
            .copied()
            .filter(|feature| {
                features.contains(feature)
                    && !(feature.repo_local_only() && scope != Scope::Repository)
            })
            .collect();

        // The `pixel` skill lands in each selected harness's skills dir (the
        // installer writes it, so the review's dry run already lists every
        // path); the notes are only for the agents nothing reaches that way.
        let mut notes = Vec::new();
        for agent in &agents {
            match agent {
                AgentTarget::Pi if scope == Scope::Global => notes.push(format!(
                    "{} gets the pixel skill and the impact/brief package under \
                     ~/.pi/agent.",
                    agent.name()
                )),
                AgentTarget::AgentMd if scope == Scope::Global => notes.push(format!(
                    "{} gets the pixel skill under ~/.agents/skills, which \
                     GitHub Copilot CLI and Amp read; an agent that reads \
                     only AGENTS.md gets nothing.",
                    agent.name()
                )),
                AgentTarget::AgentMd => notes.push(format!(
                    "{} names no harness directory in a repository; nothing \
                     is written for it.",
                    agent.name()
                )),
                _ => {}
            }
        }

        let mut config_writes: Vec<ConfigWrite> = features
            .iter()
            .filter_map(|feature| match feature.writes() {
                Writes::Config(key, value) => Some(ConfigWrite { key, value }),
                Writes::Hooks(_) => None,
            })
            .collect();
        // The retrieval-first feature is also the brief feature: it installs the
        // prompt-submit hook and enables the per-prompt brief in one choice.
        if features.contains(&Feature::Prompt) {
            config_writes.insert(
                0,
                ConfigWrite {
                    key: &["brief"],
                    value: ConfigValue::Bool(true),
                },
            );
        }

        Ok(Self {
            base: base.to_path_buf(),
            scope,
            agents,
            features,
            config_writes,
            notes,
        })
    }

    /// What the installer is asked to deploy for this plan: the selected
    /// agents, and the hook groups the selected features switch on. The hook
    /// and skill files are not rendered here; the installer owns them, and
    /// the review prints its dry run.
    pub fn install_selection(&self) -> InstallSelection {
        let has = |group| {
            self.features
                .iter()
                .any(|feature| feature.writes() == Writes::Hooks(group))
        };
        InstallSelection {
            agents: self.agents.clone(),
            task_hooks: has(HookGroup::Task),
            codex_hooks: has(HookGroup::Codex) || self.agents.contains(&AgentTarget::Codex),
            // pi extension is installed automatically when pi is selected.
            pi_extension: has(HookGroup::Pi) || self.agents.contains(&AgentTarget::Pi),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from("/tmp/pixel-setup-plan-test")
    }

    fn make_plan(scope: Scope, agents: &[AgentTarget], features: &[Feature]) -> SetupPlan {
        SetupPlan::new(&base(), scope, agents, features).unwrap()
    }

    #[test]
    fn the_selected_agents_and_features_are_normalized_to_catalog_order() {
        let plan = make_plan(
            Scope::Repository,
            &[AgentTarget::Pi, AgentTarget::ClaudeCode],
            &[Feature::Metrics, Feature::Prompt],
        );
        assert_eq!(
            plan.agents,
            vec![AgentTarget::ClaudeCode, AgentTarget::Pi],
            "the wizard's answer order must not reach the output"
        );
        assert_eq!(plan.features, vec![Feature::Prompt, Feature::Metrics]);
    }

    #[test]
    fn the_agents_nothing_reaches_become_notes() {
        let plan = make_plan(Scope::Global, &[AgentTarget::Pi], &[]);
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("impact/brief package")),
            "pi's package note: {:?}",
            plan.notes
        );

        let plan = make_plan(Scope::Global, &[AgentTarget::AgentMd], &[]);
        assert_eq!(
            plan.notes,
            vec![
                "AGENTS.md gets the pixel skill under ~/.agents/skills, which GitHub Copilot \
                 CLI and Amp read; an agent that reads only AGENTS.md gets nothing."
                    .to_string()
            ],
            "the generic target names where its skill goes"
        );
        let plan = make_plan(Scope::Repository, &[AgentTarget::AgentMd], &[]);
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("nothing is written")),
            "a repository run writes no skill: {:?}",
            plan.notes
        );
    }

    #[test]
    fn a_hook_feature_selects_what_the_installer_deploys() {
        let plan = make_plan(
            Scope::Repository,
            &[AgentTarget::ClaudeCode, AgentTarget::Pi],
            &[Feature::Prompt],
        );
        assert_eq!(
            plan.install_selection(),
            InstallSelection {
                agents: vec![AgentTarget::ClaudeCode, AgentTarget::Pi],
                task_hooks: true,
                codex_hooks: false,
                pi_extension: true,
            },
            "the retrieval-first feature installs task hooks and pi is automatic"
        );

        let none = make_plan(
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Classify],
        )
        .install_selection();
        assert!(
            !none.task_hooks && !none.codex_hooks && !none.pi_extension,
            "a feature left off installs nothing: {none:?}"
        );

        let codex = make_plan(Scope::Global, &[AgentTarget::Codex], &[Feature::Classify])
            .install_selection();
        assert!(
            codex.codex_hooks && !codex.task_hooks && !codex.pi_extension,
            "Codex hooks are installed automatically when Codex is selected: {codex:?}"
        );
    }

    #[test]
    fn the_configuration_keys_are_collected_in_catalog_order() {
        let plan = make_plan(
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Metrics, Feature::Classify, Feature::Prompt],
        );
        let keys: Vec<&[&str]> = plan.config_writes.iter().map(|w| w.key).collect();
        assert_eq!(
            keys,
            vec![
                &["brief"][..],
                &["metrics"][..],
                &["classify", "enabled"][..]
            ]
        );
    }
}
