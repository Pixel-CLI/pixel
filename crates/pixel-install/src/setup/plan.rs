// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What a setup will write, decided before anything is written.
//!
//! The split is the point: a wizard that prompts, writes and reports at the
//! same time cannot show the user what it is about to do. `SetupPlan` resolves
//! every answer to concrete paths first, so the review screen prints the exact
//! files and the exact text, cancelling writes nothing, and applying is a
//! separate step over a plan that is already known to be valid.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{InstallError, Result};

use super::agent::{AgentTarget, Scope, join};
use super::feature::{ConfigValue, Feature, Writes};
use super::render::render_block;

/// The rule file `pixel setup` writes inside a harness's rules directory,
/// relative to the scope base. Named for what it is: a setup block, not a rule
/// the user edits.
pub const RULES_FILE: &str = "pixel-setup.md";

/// The rules directories, relative to the scope base, of the harnesses that
/// load a per-tool rule file. A harness with no directory here gets its
/// steering in its instruction file only.
const RULES_DIRS: &[(AgentTarget, &str)] = &[
    (AgentTarget::ClaudeCode, ".claude/rules"),
    (AgentTarget::Codex, ".codex/rules"),
    (AgentTarget::Cursor, ".cursor/rules"),
    (AgentTarget::Gemini, ".gemini/rules"),
    (AgentTarget::Devin, ".devin/rules"),
];

/// An instruction file the setup rewrites, and the agents that read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionWrite {
    /// The file to upsert the block into.
    pub path: PathBuf,
    /// The agents whose instruction file this is, in catalog order.
    pub agents: Vec<AgentTarget>,
}

/// A per-agent rule file the setup writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleWrite {
    /// The file to write.
    pub path: PathBuf,
    /// The harness whose rules directory it sits in.
    pub agent: AgentTarget,
}

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
    /// The managed block, rendered once for every file below.
    pub block: String,
    /// The instruction files to rewrite.
    pub instruction_writes: Vec<InstructionWrite>,
    /// The rule files to write, empty unless the `rules` feature is selected.
    pub rule_writes: Vec<RuleWrite>,
    /// The configuration keys to store, applied by the caller: this crate does
    /// not own the global YAML document.
    pub config_writes: Vec<ConfigWrite>,
    /// What the user has to do by hand, and what another pixel command owns.
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

        let mut by_path: BTreeMap<PathBuf, Vec<AgentTarget>> = BTreeMap::new();
        let mut notes = Vec::new();
        for agent in &agents {
            match agent.instruction_path(base, scope) {
                Some(path) => by_path.entry(path).or_default().push(*agent),
                None => notes.push(format!(
                    "{} has no instruction file pixel writes in this scope; \
                     copy the block below into the file it reads by hand.",
                    agent.name()
                )),
            }
        }

        let rule_writes = rule_writes(base, &features, &agents);
        let config_writes = features
            .iter()
            .filter_map(|feature| match feature.writes() {
                Writes::Config(key, value) => Some(ConfigWrite { key, value }),
                _ => None,
            })
            .collect();
        for feature in &features {
            if let Writes::Note(note) = feature.writes() {
                notes.push(format!("{}: {note}", feature.label()));
            }
        }

        Ok(Self {
            base: base.to_path_buf(),
            scope,
            agents,
            block: render_block(&features),
            features,
            instruction_writes: by_path
                .into_iter()
                .map(|(path, agents)| InstructionWrite { path, agents })
                .collect(),
            rule_writes,
            config_writes,
            notes,
        })
    }

    /// Write the block into every instruction file, and the rules files when
    /// the `rules` feature is selected.
    ///
    /// Returns the paths written, in the order they were written. A failure
    /// stops here: the files already written hold an idempotent block, so
    /// re-running finishes the job, and the review screen promised nothing
    /// beyond them.
    pub fn apply(&self) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for write in &self.instruction_writes {
            super::files::upsert_file(&write.path, &self.block).map_err(|err| {
                InstallError::Setup(format!("update {}: {err}", write.path.display()))
            })?;
            written.push(write.path.clone());
        }
        for write in &self.rule_writes {
            super::files::upsert_file(&write.path, &self.block).map_err(|err| {
                InstallError::Setup(format!("update {}: {err}", write.path.display()))
            })?;
            written.push(write.path.clone());
        }
        Ok(written)
    }

    /// Whether the plan writes anything at all. A setup whose every answer was
    /// a note has nothing to do, and saying so is better than reporting a
    /// success that changed nothing.
    pub fn is_empty(&self) -> bool {
        self.instruction_writes.is_empty() && self.rule_writes.is_empty()
    }
}

/// Every rule file a setup could write under `base`, whatever the selection:
/// `pixel uninstall` needs the list without the answers that produced it.
pub fn rule_files(base: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = RULES_DIRS
        .iter()
        .map(|(_, dir)| join(base, &[dir, RULES_FILE]))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The rule files the `rules` feature writes, one per selected agent that has
/// a rules directory. Duplicates collapse: two agents sharing a directory get
/// one file.
fn rule_writes(base: &Path, features: &[Feature], agents: &[AgentTarget]) -> Vec<RuleWrite> {
    if !features.contains(&Feature::Rules) {
        return Vec::new();
    }
    let mut seen = BTreeMap::new();
    for (agent, dir) in RULES_DIRS {
        if agents.contains(agent) {
            seen.entry(*agent)
                .or_insert_with(|| join(base, &[dir, RULES_FILE]));
        }
    }
    seen.into_iter()
        .map(|(agent, path)| RuleWrite { path, agent })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from("/tmp/pixel-setup-plan-test")
    }

    fn plan(scope: Scope, agents: &[AgentTarget], features: &[Feature]) -> SetupPlan {
        SetupPlan::new(&base(), scope, agents, features).unwrap()
    }

    #[test]
    fn one_shared_instruction_file_is_written_once_for_every_agent_reading_it() {
        let plan = plan(
            Scope::Repository,
            &[
                AgentTarget::Cursor,
                AgentTarget::Devin,
                AgentTarget::AgentMd,
            ],
            &[Feature::Prompt],
        );
        assert_eq!(plan.instruction_writes.len(), 1, "got {plan:?}");
        let write = &plan.instruction_writes[0];
        assert_eq!(write.path, base().join("AGENTS.md"));
        assert_eq!(
            write.agents,
            vec![
                AgentTarget::Devin,
                AgentTarget::Cursor,
                AgentTarget::AgentMd
            ],
            "the review screen names every agent reading the file"
        );
    }

    #[test]
    fn the_selected_agents_and_features_are_normalized_to_catalog_order() {
        let plan = plan(
            Scope::Repository,
            &[AgentTarget::Pi, AgentTarget::ClaudeCode],
            &[Feature::Semantic, Feature::Prompt],
        );
        assert_eq!(
            plan.agents,
            vec![AgentTarget::ClaudeCode, AgentTarget::Pi],
            "the wizard's answer order must not reach the output"
        );
        assert_eq!(plan.features, vec![Feature::Prompt, Feature::Semantic]);
    }

    #[test]
    fn a_repository_local_feature_is_dropped_outside_a_repository() {
        let global = plan(Scope::Global, &[AgentTarget::ClaudeCode], &[Feature::Land]);
        assert!(global.features.is_empty(), "got {:?}", global.features);
        assert!(!global.block.contains("Publishing"));

        let repo = plan(
            Scope::Repository,
            &[AgentTarget::ClaudeCode],
            &[Feature::Land],
        );
        assert_eq!(repo.features, vec![Feature::Land]);
        assert!(repo.block.contains("### Publishing"));
    }

    #[test]
    fn an_agent_with_no_instruction_file_in_scope_becomes_a_note() {
        let plan = plan(Scope::Global, &[AgentTarget::Cursor], &[Feature::Prompt]);
        assert!(plan.instruction_writes.is_empty());
        assert!(
            plan.notes.iter().any(|note| note.contains("Cursor")),
            "got {:?}",
            plan.notes
        );
        assert!(plan.is_empty(), "nothing to write, and it says so");
    }

    #[test]
    fn a_feature_that_another_command_owns_becomes_a_note_and_writes_nothing() {
        let plan = plan(
            Scope::Repository,
            &[AgentTarget::ClaudeCode],
            &[Feature::Guard, Feature::Prompt],
        );
        assert!(
            plan.notes.iter().any(|note| note.contains("pixel install")),
            "got {:?}",
            plan.notes
        );
        assert_eq!(plan.instruction_writes.len(), 1);
    }

    #[test]
    fn the_configuration_keys_are_collected_in_catalog_order() {
        let plan = plan(
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Metrics, Feature::Classify, Feature::Brief],
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

    #[test]
    fn no_rule_file_is_written_unless_the_rules_feature_is_selected() {
        let without = plan(
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Prompt],
        );
        assert!(without.rule_writes.is_empty());

        let with = plan(Scope::Global, &[AgentTarget::ClaudeCode], &[Feature::Rules]);
        assert_eq!(
            with.rule_writes,
            vec![RuleWrite {
                path: base().join(".claude/rules").join(RULES_FILE),
                agent: AgentTarget::ClaudeCode,
            }]
        );
    }

    #[test]
    fn two_agents_sharing_a_rules_directory_write_one_file() {
        let plan = plan(
            Scope::Global,
            &[AgentTarget::Gemini, AgentTarget::Antigravity],
            &[Feature::Rules],
        );
        assert_eq!(plan.rule_writes.len(), 1, "got {:?}", plan.rule_writes);
    }

    #[test]
    fn applying_writes_the_block_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let plan = SetupPlan::new(
            dir.path(),
            Scope::Repository,
            &[AgentTarget::ClaudeCode, AgentTarget::Cursor],
            &[Feature::Prompt],
        )
        .unwrap();

        let written = plan.apply().unwrap();
        assert_eq!(
            written,
            vec![dir.path().join("AGENTS.md"), dir.path().join("CLAUDE.md")],
            "instruction files are written in path order, so a run is reproducible"
        );
        for path in &written {
            let content = std::fs::read_to_string(path).unwrap();
            assert_eq!(content, plan.block, "the file is exactly the block");
        }

        // A user's own line survives a re-run.
        let agents_md = dir.path().join("AGENTS.md");
        let with_text = format!(
            "# House rules\n\n{content}",
            content = std::fs::read_to_string(&agents_md).unwrap()
        );
        std::fs::write(&agents_md, with_text).unwrap();
        plan.apply().unwrap();
        let after = std::fs::read_to_string(&agents_md).unwrap();
        assert!(after.starts_with("# House rules\n"), "got {after}");
        assert_eq!(after.matches(super::super::files::BLOCK_START).count(), 1);
    }

    #[test]
    fn a_malformed_block_stops_the_apply_instead_of_guessing() {
        let dir = tempfile::tempdir().unwrap();
        // AGENTS.md sorts first, so the run stops before writing anything else.
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# rules\n<!-- pixel:setup:start -->\n",
        )
        .unwrap();
        let plan = SetupPlan::new(
            dir.path(),
            Scope::Repository,
            &[AgentTarget::ClaudeCode, AgentTarget::Cursor],
            &[Feature::Prompt],
        )
        .unwrap();
        let err = plan.apply().unwrap_err();
        assert!(format!("{err}").contains("marker"), "got {err}");
        assert!(
            !dir.path().join("CLAUDE.md").exists(),
            "the malformed file is the user's to fix; the run must not continue past it"
        );
    }
}
