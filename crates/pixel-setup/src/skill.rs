// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The `pixel` skill, deployed to every configured harness.
//!
//! One `skills/<name>/SKILL.md` under a harness's configuration root is how
//! every agent loads extra know-how, so the retrieval workflow lands as a
//! skill the agent reads on demand — not as text injected into its prompt.
//! The deploy mirrors the classify helpers' (`config::SKILL_ROOTS`): an
//! unattended install writes `.claude` plus every harness whose
//! configuration directory already exists; a guided run writes only the
//! selected agents' roots. Pi is not a root here — its package step writes
//! the identical file itself.

use std::fs;
use std::path::{Path, PathBuf};

use crate::InstallError;
use crate::config::SKILL_ROOTS;
use crate::install::{
    CheckStatus, InstallSelection, InstallStep, dry_run_summary, write_atomically,
};
use crate::setup::AgentTarget;

type Result<T> = std::result::Result<T, InstallError>;

/// The skill content: one front matter and prompt shared by every harness
/// that reads a skill, so they all read one source.
pub const PIXEL_SKILL: &str = concat!(
    "---\nname: pixel\ndescription: >\n  Finds where a symbol is defined, who calls it, and what breaks if it changes,\n  via the `pixel` CLI code graph. Use before renaming, changing a signature or\n  deleting a function or type; when asked who calls or uses X, where X is\n  defined, or where some behaviour lives without a known name; instead of\n  grepping for `name(` or `fn name`. Also outlines big files, packs one\n  symbol's source and scopes tasks. Needs `pixel` on PATH and a git repo\n  (`pixel prepare-repo` builds the index once).\nlicense: MIT\n---\n\n",
    include_str!("../assets/pixel-agent-prompt.md"),
);

/// The install step's id in a report.
const STEP_ID: &str = "skills.pixel";

/// The skill's path inside one harness root.
const SKILL_REL: &str = "skills/pixel/SKILL.md";

/// The harness configuration root a selected agent's skill copy lands in,
/// relative to home. Pi's package step writes the identical file itself and
/// the generic `AGENTS.md` target names no harness directory.
fn skill_root(agent: AgentTarget) -> Option<&'static str> {
    Some(match agent {
        AgentTarget::ClaudeCode => ".claude",
        AgentTarget::Codex => ".codex",
        AgentTarget::Devin => ".devin",
        AgentTarget::Cursor => ".cursor",
        AgentTarget::Gemini | AgentTarget::Antigravity => ".gemini",
        AgentTarget::OpenCode => ".config/opencode",
        AgentTarget::Pi | AgentTarget::AgentMd => return None,
    })
}

/// The harness roots whose `skills/pixel/SKILL.md` this install writes.
///
/// A guided run touches the selected agents' roots only — selected means
/// configured, so the directory is created when absent. The unattended
/// install adds no directory of its own: `.claude` always (its hooks land
/// there regardless) and every `SKILL_ROOTS` entry that already exists.
fn skill_dirs(home: &Path, selection: Option<&InstallSelection>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = match selection {
        Some(selection) => selection
            .agents
            .iter()
            .filter_map(|agent| skill_root(*agent))
            .map(|root| home.join(root))
            .collect(),
        None => [home.join(".claude")]
            .into_iter()
            .chain(
                SKILL_ROOTS
                    .iter()
                    .map(|root| home.join(root))
                    .filter(|dir| dir.is_dir()),
            )
            .collect(),
    };
    roots.sort();
    roots.dedup();
    roots
}

/// The skill files under [`skill_dirs`] — also what uninstall sweeps, so a
/// managed copy an old guided run left under an unselected root still goes.
pub(crate) fn skill_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter().map(|dir| dir.join(SKILL_REL)).collect()
}

/// Write `skills/pixel/SKILL.md` into every configured harness's skills
/// dir; a differing copy is the user's, backed up before overwriting.
pub(crate) fn install(
    home: &Path,
    selection: Option<&InstallSelection>,
    dry_run: bool,
) -> Result<InstallStep> {
    let files = skill_files(&skill_dirs(home, selection));
    if dry_run {
        return Ok(InstallStep {
            id: STEP_ID.into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(
                dry_run,
                &if files.is_empty() {
                    "no configured harness skills dir".to_string()
                } else {
                    format!("deploy the pixel skill to {} harness dir(s)", files.len())
                },
            ),
            detail: (!files.is_empty()).then(|| {
                files
                    .iter()
                    .map(|file| file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        });
    }
    for file in &files {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        // The skill is a harness file, not pixel's own: `write_atomically`
        // backs a differing copy up before overwriting.
        write_atomically(file, PIXEL_SKILL)?;
    }
    Ok(InstallStep {
        id: STEP_ID.into(),
        status: CheckStatus::Green,
        summary: format!("pixel skill deployed to {} harness dir(s)", files.len()),
        detail: (!files.is_empty()).then(|| {
            files
                .iter()
                .map(|file| file.display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        }),
    })
}

/// Every `skills/pixel` dir a pixel install may have written, for uninstall:
/// `.claude` plus every `SKILL_ROOTS` entry, whether configured or not.
pub(crate) fn managed_skill_dirs(home: &Path) -> Vec<PathBuf> {
    [PathBuf::from(".claude")]
        .into_iter()
        .chain(SKILL_ROOTS.iter().map(PathBuf::from))
        .map(|root| home.join(root).join("skills/pixel"))
        .collect()
}

// -------------------------------------------------------------------------
// Plugin-shipped skills (#1142)
// -------------------------------------------------------------------------

/// The skill names `pixel setup` and the classify helpers manage: a plugin
/// delivering under one of them would clobber what setup wrote at install
/// and take it away at uninstall, so the delivery refuses the name.
const CORE_MANAGED_SKILLS: [&str; 2] = ["pixel", "pixel-classify"];

/// What a plugin skill's sweep did.
#[derive(Debug, PartialEq, Eq)]
pub struct PluginSkillSweep {
    /// The `SKILL.md` copies that still matched what was delivered: removed.
    pub removed: Vec<PathBuf>,
    /// The `SKILL.md` copies present whose text differs from what was
    /// delivered — a user-edited copy, left in place.
    pub kept: Vec<PathBuf>,
}

/// Deliver a plugin's skill as `skills/<plugin>/SKILL.md` under `.claude`
/// and every existing `SKILL_ROOTS` entry — the unattended install's root
/// rule, the same set the core `pixel` skill is written to. Each copy goes
/// through [`write_atomically`]: a differing file there is the user's and
/// is backed up before it is overwritten, an identical one is left alone.
///
/// Returns every destination written, in a stable order, for the caller to
/// print.
///
/// # Errors
///
/// [`InstallError::Setup`] when `plugin` is not a valid plugin name or is
/// named like a core-managed skill, and the I/O errors of the writes.
pub fn deploy_plugin_skill(home: &Path, plugin: &str, content: &str) -> Result<Vec<PathBuf>> {
    if !pixel_plugin::valid_name(plugin) || CORE_MANAGED_SKILLS.contains(&plugin) {
        return Err(InstallError::Setup(format!(
            "plugin name `{plugin}` cannot name a skill: use lowercase letters, digits, `-` and `_`, and not a core-managed skill name ({})",
            CORE_MANAGED_SKILLS.join(", ")
        )));
    }
    let mut written = Vec::new();
    for dir in skill_dirs(home, None) {
        let file = dir.join("skills").join(plugin).join("SKILL.md");
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomically(&file, content)?;
        written.push(file);
    }
    Ok(written)
}

/// Remove what [`deploy_plugin_skill`] may have written for `plugin`:
/// `.claude` and every `SKILL_ROOTS` entry, whether configured or not — a
/// root a delivery skipped holds no file of the plugin's. A copy still
/// matching the text delivered at install (`expected`) goes; anything else
/// under the same name is the user's and stays, reported as kept.
pub fn remove_plugin_skill(home: &Path, plugin: &str, expected: &str) -> Result<PluginSkillSweep> {
    let mut sweep = PluginSkillSweep {
        removed: Vec::new(),
        kept: Vec::new(),
    };
    for dir in [PathBuf::from(".claude")]
        .into_iter()
        .chain(SKILL_ROOTS.iter().map(PathBuf::from))
        .map(|root| home.join(root).join("skills").join(plugin))
    {
        // A name that is not a directory (a user's symlink, say) is never
        // treated as ours: removing through it would follow the link and
        // delete outside `skills/`.
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let file = dir.join("SKILL.md");
        if fs::read_to_string(&file).is_ok_and(|text| text == expected) {
            fs::remove_file(&file)?;
            // The dir goes when it is empty; anything a user added under
            // `skills/<plugin>/` keeps it.
            if fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none()) {
                fs::remove_dir(&dir)?;
            }
            sweep.removed.push(file);
        } else if fs::symlink_metadata(&file).is_ok() {
            sweep.kept.push(file);
        }
    }
    Ok(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(agents: &[AgentTarget]) -> InstallSelection {
        InstallSelection {
            agents: agents.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn the_unattended_install_deploys_to_claude_and_every_configured_harness() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".cursor")).unwrap();

        let step = install(home, None, false).unwrap();
        assert_eq!(step.id, STEP_ID);
        assert_eq!(
            fs::read_to_string(home.join(".claude/skills/pixel/SKILL.md")).unwrap(),
            PIXEL_SKILL
        );
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/pixel/SKILL.md")).unwrap(),
            PIXEL_SKILL
        );
        assert_eq!(
            fs::read_to_string(home.join(".cursor/skills/pixel/SKILL.md")).unwrap(),
            PIXEL_SKILL
        );
        // A harness never configured gets no directory of its own.
        assert!(!home.join(".gemini").exists());
        assert!(!home.join(".pi").exists());
    }

    #[test]
    fn a_guided_run_deploys_only_to_the_selected_agents_roots() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        // Configured but not selected: nothing lands there.
        fs::create_dir_all(home.join(".codex")).unwrap();
        install(
            home,
            Some(&selection(&[AgentTarget::Cursor, AgentTarget::Pi])),
            false,
        )
        .unwrap();
        assert!(home.join(".cursor/skills/pixel/SKILL.md").is_file());
        assert!(!home.join(".codex/skills").exists());
        assert!(!home.join(".claude/skills").exists());
        // Pi's copy is its package step's, not this one's.
        assert!(!home.join(".pi").exists());
    }

    #[test]
    fn a_differing_skill_is_backed_up_not_clobbered() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let file = home.join(".claude/skills/pixel/SKILL.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "mine\n").unwrap();

        install(home, None, false).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), PIXEL_SKILL);
        let backups: Vec<_> = fs::read_dir(home.join(".claude/skills/pixel"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".pixel-bak."))
            .collect();
        assert_eq!(backups.len(), 1, "the user's copy is backed up once");
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let step = install(home, None, true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!home.join(".claude").exists());
    }

    #[test]
    fn a_plugin_skill_is_delivered_to_claude_and_every_configured_harness() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let written = deploy_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(
            written,
            [
                home.join(".claude/skills/flow/SKILL.md"),
                home.join(".codex/skills/flow/SKILL.md"),
            ]
        );
        assert_eq!(
            fs::read_to_string(home.join(".claude/skills/flow/SKILL.md")).unwrap(),
            "know-how\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/flow/SKILL.md")).unwrap(),
            "know-how\n"
        );
        assert!(
            !home.join(".gemini").exists(),
            "a never-configured harness gets no directory"
        );
    }

    #[test]
    fn a_differing_plugin_skill_copy_is_backed_up_and_an_identical_one_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let file = home.join(".codex/skills/flow/SKILL.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "mine\n").unwrap();

        deploy_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "know-how\n");
        let count_backups = |dir: &Path| {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().contains(".pixel-bak."))
                .count()
        };
        assert_eq!(
            count_backups(file.parent().unwrap()),
            1,
            "the user's copy is backed up once"
        );

        deploy_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(
            count_backups(file.parent().unwrap()),
            1,
            "delivering the same text again backs nothing up"
        );
    }

    #[test]
    fn a_plugin_skill_delivery_refuses_a_name_it_cannot_own_before_any_write() {
        for name in ["pixel", "pixel-classify", "../escape", "Flow"] {
            let home = tempfile::tempdir().unwrap();
            let err = deploy_plugin_skill(home.path(), name, "x").unwrap_err();
            assert!(
                err.to_string().contains("cannot name a skill"),
                "{name}: {err}"
            );
            assert!(
                !home.path().join(".claude").exists(),
                "{name}: nothing written on a refusal"
            );
        }
    }

    #[test]
    fn a_sweep_removes_only_copies_still_matching_what_was_delivered() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".codex")).unwrap();
        deploy_plugin_skill(home, "flow", "know-how\n").unwrap();
        // A hand-edited copy under another root is the user's.
        let edited = home.join(".codex/skills/flow/SKILL.md");
        fs::write(&edited, "mine now\n").unwrap();

        let sweep = remove_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(sweep.removed, [home.join(".claude/skills/flow/SKILL.md")]);
        assert_eq!(sweep.kept, [home.join(".codex/skills/flow/SKILL.md")]);
        assert_eq!(fs::read_to_string(&edited).unwrap(), "mine now\n");
        assert!(
            !home.join(".claude/skills/flow").exists(),
            "the emptied dir goes too"
        );
        assert!(home.join(".claude/skills").exists(), "skills/ itself stays");
    }

    #[test]
    fn a_sweep_skips_roots_never_delivered_to_and_files_a_user_added() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".claude/skills/flow")).unwrap();
        fs::write(home.join(".claude/skills/flow/SKILL.md"), "know-how\n").unwrap();
        fs::write(home.join(".claude/skills/flow/NOTES.md"), "user's\n").unwrap();

        let sweep = remove_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(
            sweep,
            PluginSkillSweep {
                removed: vec![home.join(".claude/skills/flow/SKILL.md")],
                kept: vec![],
            }
        );
        assert!(
            home.join(".claude/skills/flow/NOTES.md").is_file(),
            "a file the user added keeps the dir"
        );
        // Running the sweep again finds nothing of the plugin's left.
        assert_eq!(
            remove_plugin_skill(home, "flow", "know-how\n").unwrap(),
            PluginSkillSweep {
                removed: vec![],
                kept: vec![],
            }
        );
    }

    // A delivered copy is a real directory holding a real file. A link at
    // the dir's name points somewhere we did not write, and removing
    // through it would delete there.
    #[test]
    fn a_sweep_never_treats_a_link_at_the_skill_dir_name_as_ours() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("skills/flow"),
            home.join(".claude").join("skills").join("flow"),
        )
        .unwrap();
        std::fs::create_dir_all(elsewhere.path().join("skills/flow")).unwrap();
        std::fs::write(elsewhere.path().join("skills/flow/SKILL.md"), "know-how\n").unwrap();

        let sweep = remove_plugin_skill(home, "flow", "know-how\n").unwrap();
        assert_eq!(
            sweep,
            PluginSkillSweep {
                removed: vec![],
                kept: vec![],
            },
            "the link's target's file is not ours to remove"
        );
        assert!(elsewhere.path().join("skills/flow/SKILL.md").is_file());
    }
}
