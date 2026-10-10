// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: the interactive per-agent, per-feature onboarding wizard.
//!
//! Ported from GitButler's `but agent setup` (read at `5cbe33d`). The planning
//! half lives in [`pixel_install::setup`], which knows the agent targets, the
//! feature catalog and the managed block; this module owns the terminal, the
//! global configuration file and the report.
//!
//! The shape of the run is GitButler's: collect every answer first, resolve it
//! to a plan, print exactly what the plan would write, and only then apply.
//! Cancelling at any point writes nothing and says so.
//!
//! What is deliberately absent: repository preparation (no index, no
//! `prepare-repo`), skill-bundle installation, and any hook write. `pixel
//! install` owns the hook files, and a second writer of one settings file is
//! how a user's own hooks get lost; a feature that implies one says which
//! command deploys it instead.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use pixel_install::setup::{AgentTarget, Feature, Scope, SetupPlan};

use crate::config_cmd;

/// Where a run writes, and how its answers are read.
pub struct SetupOptions {
    /// A repository to set up instead of the machine. Its root becomes the
    /// detection base and the write base.
    pub repo: Option<PathBuf>,
    /// Print the block the default answers produce and change nothing.
    pub print: bool,
    /// The development-only switches. `TEMPORARY (dev-flags)`, see `dev`.
    pub dev: dev::Options,
}

// TEMPORARY (dev-flags): the public name `main.rs` fills in.
pub use dev::Options as DevOptions;

/// Run the wizard.
pub fn run(options: SetupOptions) -> Result<(), String> {
    let base = base_for(&options)?;
    let scope = if options.repo.is_some() {
        Scope::Repository
    } else {
        Scope::Global
    };

    if options.print {
        let plan = SetupPlan::new(
            &base,
            scope,
            &default_agents(&base, scope, running_agent()),
            &default_features(scope),
        )
        .map_err(|e| e.to_string())?;
        print!("{}", plan.block);
        return Ok(());
    }

    // TEMPORARY (dev-flags): `scripted` is `dev`'s; without it the line is
    // `if !std::io::stdin().is_terminal()`.
    let scripted = dev::scripted(&options.dev);
    if !scripted && !std::io::stdin().is_terminal() {
        return Err(
            "setup needs a terminal. Use --print for the default block, or answer both \
             --selected-agents and --selected-features to run without one."
                .into(),
        );
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stderr().lock();
    let answers = collect(&options, &base, scope, color, &mut input, &mut output)?;
    let Some(answers) = answers else {
        writeln!(output).map_err(io)?;
        writeln!(
            output,
            "Cancelled. Nothing was written and no setting was changed."
        )
        .map_err(io)?;
        return Ok(());
    };

    let plan = SetupPlan::new(&base, scope, &answers.agents, &answers.features)
        .map_err(|e| e.to_string())?;
    write_review(&mut output, &plan, color)?;
    if !scripted && !confirm(&mut input, &mut output, color)? {
        writeln!(output, "Cancelled. Nothing was written.").map_err(io)?;
        return Ok(());
    }

    let written = plan.apply().map_err(|e| e.to_string())?;
    // TEMPORARY (dev-flags): the `if` is `dev`'s; without it the loop below runs
    // unconditionally, which is what a normal run does.
    if !dev::skips_config(&options.dev) {
        for key in &plan.config_writes {
            config_cmd::set_global_key(key.key, key.value.to_json())?;
        }
    }
    report(&mut output, &plan, &written, scripted)
}

/// The directory every path of this run is resolved under.
fn base_for(options: &SetupOptions) -> Result<PathBuf, String> {
    // TEMPORARY (dev-flags): these three lines are `dev`'s; without them a run
    // always resolves `--repo` or `$HOME`.
    if let Some(root) = dev::dummy_root(&options.dev)? {
        return Ok(root);
    }
    match &options.repo {
        Some(repo) => Ok(repo.clone()),
        None => std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "setup needs $HOME, or --repo <path>".to_string()),
    }
}

/// The agents pre-checked in the picker: the harness driving this process, plus
/// every agent this scope already shows evidence of.
///
/// The running agent is a parameter, not a call to
/// [`pixel_install::setup::detect`] inside: detection reads the process
/// environment, and a test that reads the environment of the machine running
/// it asserts on the wrong thing.
fn default_agents(base: &Path, scope: Scope, running: Option<AgentTarget>) -> Vec<AgentTarget> {
    AgentTarget::ALL
        .iter()
        .copied()
        .filter(|agent| Some(*agent) == running || agent.in_use(base, scope))
        .collect()
}

/// The harness driving this process, as a setup target.
fn running_agent() -> Option<AgentTarget> {
    pixel_install::setup::detect().map(AgentTarget::from_detected)
}

/// The features pre-checked in the picker.
fn default_features(scope: Scope) -> Vec<Feature> {
    Feature::ALL
        .iter()
        .copied()
        .filter(|feature| {
            feature.default_selected() && !(feature.repo_local_only() && scope != Scope::Repository)
        })
        .collect()
}

/// The answers a run ended up with.
struct Answers {
    agents: Vec<AgentTarget>,
    features: Vec<Feature>,
}

/// Ask every question, or read the two scripted answers. `None` means the user
/// cancelled, which writes nothing.
fn collect(
    options: &SetupOptions,
    base: &Path,
    scope: Scope,
    color: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<Answers>, String> {
    let running = running_agent();
    write_intro(output, base, scope, running, color)?;
    // TEMPORARY (dev-flags): these four lines are `dev`'s; without them every
    // answer comes from the prompts below.
    if let Some(answers) = dev::answers(&options.dev, scope)? {
        return Ok(Some(answers));
    }
    let Some(agents) = ask_agents(input, output, base, scope, running, color)? else {
        return Ok(None);
    };
    let Some(features) = ask_features(input, output, scope, color)? else {
        return Ok(None);
    };
    Ok(Some(Answers { agents, features }))
}

fn write_intro(
    output: &mut impl Write,
    base: &Path,
    scope: Scope,
    detected: Option<AgentTarget>,
    color: bool,
) -> Result<(), String> {
    writeln!(output).map_err(io)?;
    writeln!(
        output,
        "{}",
        paint(color, "1;32", "pixel setup — agents and features")
    )
    .map_err(io)?;
    writeln!(output, "Writes under: {}", base.display()).map_err(io)?;
    writeln!(
        output,
        "Scope: {}",
        match scope {
            Scope::Global => "this machine ($HOME)",
            Scope::Repository => "this repository",
        }
    )
    .map_err(io)?;
    writeln!(output).map_err(io)?;
    writeln!(output, "This does:").map_err(io)?;
    writeln!(
        output,
        "  {} Write a managed pixel block into each agent's instruction file",
        paint(color, "2", "•")
    )
    .map_err(io)?;
    writeln!(
        output,
        "  {} Store the settings the features you pick need",
        paint(color, "2", "•")
    )
    .map_err(io)?;
    writeln!(output).map_err(io)?;
    writeln!(
        output,
        "{}",
        paint(
            color,
            "2",
            "Nothing is written until you confirm. `q`, Esc or Ctrl-C cancels."
        )
    )
    .map_err(io)?;
    if let Some(agent) = detected {
        writeln!(
            output,
            "{}",
            paint(color, "2", &format!("Detected agent: {}", agent.name()))
        )
        .map_err(io)?;
    }
    Ok(())
}

/// The agent question: one numbered list, answered by index.
fn ask_agents(
    input: &mut impl BufRead,
    output: &mut impl Write,
    base: &Path,
    scope: Scope,
    running: Option<AgentTarget>,
    color: bool,
) -> Result<Option<Vec<AgentTarget>>, String> {
    let defaults = default_agents(base, scope, running);
    let mut lines = Vec::new();
    for agent in AgentTarget::ALL {
        let checked = defaults.contains(&agent);
        let note = if Some(agent) == running {
            " (driving now)"
        } else if checked {
            " (detected)"
        } else {
            ""
        };
        lines.push(format!(
            "  {}{} {}{}",
            paint(color, "1", &format!("{}.", agent.index())),
            if checked { "x" } else { " " },
            agent.name(),
            note
        ));
    }
    let picked = ask_list(
        input,
        output,
        "Which agents should pixel write for?",
        &lines,
        &indices(&defaults, |agent| agent.index()),
        color,
    )?;
    let Some(indices) = picked else {
        return Ok(None);
    };
    let agents: Vec<AgentTarget> = AgentTarget::ALL
        .iter()
        .copied()
        .filter(|agent| indices.contains(&agent.index()))
        .collect();
    if agents.is_empty() {
        return Err("setup needs at least one agent. Re-run and pick one.".into());
    }
    Ok(Some(agents))
}

/// The feature question: one numbered list, answered by index.
fn ask_features(
    input: &mut impl BufRead,
    output: &mut impl Write,
    scope: Scope,
    color: bool,
) -> Result<Option<Vec<Feature>>, String> {
    let defaults = default_features(scope);
    let mut lines = Vec::new();
    for feature in Feature::ALL {
        let disabled = feature.repo_local_only() && scope != Scope::Repository;
        let checked = defaults.contains(&feature);
        lines.push(format!(
            "  {}{} {}{}",
            paint(color, "1", &format!("{}.", feature.index())),
            if checked { "x" } else { " " },
            feature.label(),
            if disabled {
                " (this scope cannot use it; re-run with --repo <path>)"
            } else {
                ""
            }
        ));
    }
    let picked = ask_list(
        input,
        output,
        "Which features should be on?",
        &lines,
        &indices(&defaults, |feature| feature.index()),
        color,
    )?;
    let Some(indices) = picked else {
        return Ok(None);
    };
    Ok(Some(
        Feature::ALL
            .iter()
            .copied()
            .filter(|feature| {
                indices.contains(&feature.index())
                    && !(feature.repo_local_only() && scope != Scope::Repository)
            })
            .collect(),
    ))
}

/// A numbered list question. The answer is a list of indices; an empty line
/// keeps `default`, `all` and `none` are accepted, and `q` cancels.
fn ask_list(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    lines: &[String],
    default: &[usize],
    color: bool,
) -> Result<Option<Vec<usize>>, String> {
    let total = lines.len();
    loop {
        writeln!(output).map_err(io)?;
        for line in lines {
            writeln!(output, "{line}").map_err(io)?;
        }
        write!(
            output,
            "{} > ",
            paint(
                color,
                "1",
                &format!(
                    "{label} [{total} numbers, `all`, `none`] default [{}]",
                    default
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            )
        )
        .map_err(io)?;
        output.flush().map_err(io)?;
        let mut answer = String::new();
        if input.read_line(&mut answer).map_err(io)? == 0 {
            return Ok(None);
        }
        if answer.trim().is_empty() {
            // The header promises the bracketed list is the default, and a
            // re-ask that discarded it would fight the user for nothing.
            return Ok(Some(default.to_vec()));
        }
        match parse_indices(&answer, total) {
            Ok(Some(indices)) => return Ok(Some(indices)),
            Ok(None) => return Ok(None),
            Err(err) => {
                writeln!(output, "{err}").map_err(io)?;
            }
        }
    }
}

/// Parse an answer into indices: `all`, `none`, `q`, or a comma- or
/// space-separated list. `Err` explains what was wrong, `Ok(None)` is a cancel.
fn parse_indices(answer: &str, total: usize) -> Result<Option<Vec<usize>>, String> {
    let answer = answer.trim();
    match answer.to_ascii_lowercase().as_str() {
        "" => Err("an empty answer keeps the shown selection".into()),
        "q" => Ok(None),
        "all" => Ok(Some((1..=total).collect())),
        "none" => Ok(Some(Vec::new())),
        _ => {
            let mut indices = Vec::new();
            for token in answer.split([',', ' ']).filter(|t| !t.is_empty()) {
                let index: usize = token
                    .parse()
                    .map_err(|_| format!("{token:?} is not a number from 1 to {total}"))?;
                if index == 0 || index > total {
                    return Err(format!("{index} is outside 1 to {total}"));
                }
                if !indices.contains(&index) {
                    indices.push(index);
                }
            }
            if indices.is_empty() {
                return Err("an empty answer keeps the shown selection".into());
            }
            Ok(Some(indices))
        }
    }
}

/// The review: every path, every configuration key, every note, and the exact
/// text, before anything is written.
fn write_review(output: &mut impl Write, plan: &SetupPlan, color: bool) -> Result<(), String> {
    writeln!(output).map_err(io)?;
    writeln!(
        output,
        "{}",
        paint(
            color,
            "1;32",
            &format!("pixel setup will write under {}", plan.base.display())
        )
    )
    .map_err(io)?;
    if !plan.instruction_writes.is_empty() {
        writeln!(output, "Instruction files:").map_err(io)?;
        for write in &plan.instruction_writes {
            let agents = write
                .agents
                .iter()
                .map(|agent| agent.name())
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                output,
                "  {} {}{}",
                paint(color, "2", "•"),
                write.path.display(),
                paint(color, "2", &format!("  ({agents})"))
            )
            .map_err(io)?;
        }
    }
    if !plan.rule_writes.is_empty() {
        writeln!(output, "Rule files:").map_err(io)?;
        for write in &plan.rule_writes {
            writeln!(
                output,
                "  {} {}  ({})",
                paint(color, "2", "•"),
                write.path.display(),
                write.agent.name()
            )
            .map_err(io)?;
        }
    }
    if !plan.config_writes.is_empty() {
        writeln!(output, "Global settings (~/.pixel/config.yaml):").map_err(io)?;
        for key in &plan.config_writes {
            writeln!(
                output,
                "  {} {}: {}",
                paint(color, "2", "•"),
                key.key.join("."),
                render_value(key.value.to_json())
            )
            .map_err(io)?;
        }
    }
    if !plan.notes.is_empty() {
        writeln!(output).map_err(io)?;
        writeln!(output, "Not written here:").map_err(io)?;
        for note in &plan.notes {
            writeln!(output, "  {} {note}", paint(color, "2", "•")).map_err(io)?;
        }
    }
    writeln!(output).map_err(io)?;
    writeln!(output, "The exact text:").map_err(io)?;
    for line in plan.block.lines() {
        writeln!(output, "  {line}").map_err(io)?;
    }
    Ok(())
}

/// The one question between the review and the writes.
fn confirm(input: &mut impl BufRead, output: &mut impl Write, color: bool) -> Result<bool, String> {
    write!(
        output,
        "{} ",
        paint(color, "1;32", "Write these files and settings? [Y/n]")
    )
    .map_err(io)?;
    output.flush().map_err(io)?;
    let mut line = String::new();
    if input.read_line(&mut line).map_err(io)? == 0 {
        return Ok(false);
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Ok(true),
        _ => Ok(false),
    }
}

/// What the run did, after it did it.
fn report(
    output: &mut impl Write,
    plan: &SetupPlan,
    written: &[PathBuf],
    scripted: bool,
) -> Result<(), String> {
    writeln!(output).map_err(io)?;
    if written.is_empty() && plan.config_writes.is_empty() {
        writeln!(output, "pixel setup wrote nothing.").map_err(io)?;
    } else {
        writeln!(output, "pixel setup complete:").map_err(io)?;
        for path in written {
            writeln!(output, "  updated {}", path.display()).map_err(io)?;
        }
        for key in &plan.config_writes {
            writeln!(
                output,
                "  set {} = {}",
                key.key.join("."),
                render_value(key.value.to_json())
            )
            .map_err(io)?;
        }
    }
    for note in &plan.notes {
        writeln!(output, "  note: {note}").map_err(io)?;
    }
    if !scripted {
        writeln!(
            output,
            "Run `pixel doctor .` afterwards to check the machine end to end."
        )
        .map_err(io)?;
    }
    Ok(())
}

/// A configuration value as the YAML document would show it.
fn render_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::Bool(on) => on.to_string(),
        // A YAML scalar that reads as a keyword (`off`, `true`) has to stay a
        // string, so it is shown quoted; the value's own Display already
        // carries the quotes and must not be quoted twice.
        serde_json::Value::String(text) => format!("\"{text}\""),
        other => other.to_string(),
    }
}

/// The 1-based indices of `items`, in catalog order.
fn indices<T>(items: &[T], index: impl Fn(&T) -> usize) -> Vec<usize> {
    items.iter().map(index).collect()
}

/// Wrap `text` in the SGR `code` when `color` is on, pass it through otherwise.
fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// A write error, reported the way every other pixel command reports one.
fn io(err: std::io::Error) -> String {
    err.to_string()
}

// ===========================================================================
// TEMPORARY (dev-flags): the development-only switches.
//
// `but agent setup` has no non-interactive apply, so the port carries three
// throwaway flags to drive the wizard from a script while the goldens under
// `tests/setup/` are built. They are hidden from `--help` and are not part of
// the shipped command.
//
// REMOVAL (before the release, tracked in docs/design/pixel-setup-plan.md and
// on issue #890). Every site is a deletion; nothing new is written:
//
//   1. delete this whole section, to the matching END marker below, and the
//      `#[cfg(test)] mod dev_tests` block in the tests module;
//   2. delete the `dev: dev::Options` field of `SetupOptions` here, the
//      `dev` argument at the construction site in `main.rs`, and the three
//      `--selected-agents` / `--selected-features` / `--dummy-apply` clap args
//      in `main.rs` (marked with the same phrase);
//   3. delete the four `TEMPORARY (dev-flags)` lines in `run` and `collect`,
//      and the `dev::dummy_root` / `dev::skips_config` blocks they guard:
//      each one wraps the production code, so deleting the wrapper leaves it;
//   4. delete the scripted cases in `crates/pixel/tests/cli/setup_cli.rs`
//      (marked the same way) and the `tests/setup/` gitignore entry, if any.
//
// Nothing else refers to them: `setup::goldens` renders and applies in-process
// and the golden files are compared without the command line.
// ===========================================================================
mod dev {
    use std::path::{Path, PathBuf};

    use pixel_install::setup::{AgentTarget, Feature, Scope};

    use super::{Answers, parse_indices};

    /// The directory `--dummy-apply` writes under, relative to the repository
    /// root.
    const DUMMY_ROOT: [&str; 2] = ["tests", "setup"];

    /// The three development switches, as `main.rs` fills them from the
    /// command line.
    #[derive(Debug, Clone, Default)]
    pub struct Options {
        /// Answer the agent question from 1-based indices, comma-separated.
        pub selected_agents: Option<String>,
        /// Answer the feature question from 1-based indices, comma-separated.
        pub selected_features: Option<String>,
        /// Redirect every write under `tests/setup/` instead of a real home or
        /// repository.
        pub dummy_apply: bool,
    }

    /// The directory `--dummy-apply` writes under, relative to the repository
    /// root. `None` for a normal run.
    pub(super) fn dummy_root(options: &Options) -> Result<Option<PathBuf>, String> {
        if !options.dummy_apply {
            return Ok(None);
        }
        let root = crate::discover_root(Path::new("."))?;
        Ok(Some(
            DUMMY_ROOT.iter().fold(root, |path, part| path.join(part)),
        ))
    }

    /// Whether a dummy run must leave the real `~/.pixel/config.yaml` alone:
    /// the promise of the flag is that nothing outside `tests/setup/` changes.
    pub(super) fn skips_config(options: &Options) -> bool {
        options.dummy_apply
    }

    /// Whether *both* answers came from the command line. One flag and a
    /// terminal still has a question to ask; one flag without a terminal has no
    /// way to ask it.
    pub(super) fn scripted(options: &Options) -> bool {
        options.selected_agents.is_some() && options.selected_features.is_some()
    }

    /// The answers the flags carry, or `None` when the wizard has to ask.
    pub(super) fn answers(options: &Options, scope: Scope) -> Result<Option<Answers>, String> {
        let (Some(agents), Some(features)) = (&options.selected_agents, &options.selected_features)
        else {
            return Ok(None);
        };
        Ok(Some(Answers {
            agents: parse_agents(agents)?,
            features: parse_features(features, scope)?,
        }))
    }

    /// `--selected-agents 1,3,4`.
    fn parse_agents(list: &str) -> Result<Vec<AgentTarget>, String> {
        let total = AgentTarget::ALL.len();
        let Some(indices) = parse_indices(list, total)? else {
            return Err("--selected-agents cannot cancel a run".into());
        };
        Ok(AgentTarget::ALL
            .iter()
            .copied()
            .filter(|agent| indices.contains(&agent.index()))
            .collect())
    }

    /// `--selected-features 2,4,12`.
    fn parse_features(list: &str, scope: Scope) -> Result<Vec<Feature>, String> {
        let total = Feature::ALL.len();
        let Some(indices) = parse_indices(list, total)? else {
            return Err("--selected-features cannot cancel a run".into());
        };
        Ok(Feature::ALL
            .iter()
            .copied()
            .filter(|feature| {
                indices.contains(&feature.index())
                    && !(feature.repo_local_only() && scope != Scope::Repository)
            })
            .collect())
    }
}
// ===========================================================================
// END TEMPORARY (dev-flags)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with `answers` as its input and return (result, what it wrote).
    /// Concrete types, not `&mut dyn`: the prompts take `impl BufRead`/`impl
    /// Write`, which is the shape production passes them.
    fn with_input<T>(
        answers: &str,
        f: impl FnOnce(&mut std::io::Cursor<Vec<u8>>, &mut Vec<u8>) -> T,
    ) -> (T, String) {
        let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = f(&mut input, &mut output);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn the_list_prompt_asks_again_after_an_answer_it_cannot_read() {
        let (picked, written) = with_input("nonsense\n2\n", |input, output| {
            ask_list(input, output, "Pick", &["a".into(), "b".into()], &[1], true)
        });
        assert_eq!(picked.unwrap(), Some(vec![2]));
        assert!(
            written.contains("not a number"),
            "the reader has to say what was wrong: {written}"
        );
        assert_eq!(
            written.matches("Pick").count(),
            2,
            "the list is shown again: {written}"
        );
    }

    #[test]
    fn the_list_prompt_keeps_the_shown_default_on_an_empty_line() {
        let (picked, _) = with_input("\n", |input, output| {
            ask_list(
                input,
                output,
                "Pick",
                &["a".into(), "b".into()],
                &[1, 2],
                true,
            )
        });
        assert_eq!(picked.unwrap(), Some(vec![1, 2]));
    }

    #[test]
    fn the_list_prompt_cancels_on_q_and_on_end_of_input() {
        let (cancelled, _) = with_input("q\n", |input, output| {
            ask_list(input, output, "Pick", &["a".into()], &[1], true)
        });
        assert_eq!(cancelled.unwrap(), None);
        let (ended, _) = with_input("", |input, output| {
            ask_list(input, output, "Pick", &["a".into()], &[1], true)
        });
        assert_eq!(ended.unwrap(), None, "a closed stdin is not an answer");
    }

    #[test]
    fn the_confirmation_defaults_to_yes_and_refuses_everything_else() {
        for (answer, expected) in [
            ("\n", true),
            ("y\n", true),
            ("yes\n", true),
            ("Y\n", true),
            ("n\n", false),
            ("no\n", false),
            ("maybe\n", false),
            ("  \n", true),
        ] {
            let (confirmed, _) = with_input(answer, |input, output| confirm(input, output, true));
            assert_eq!(confirmed.unwrap(), expected, "{answer:?}");
        }
        let (closed, _) = with_input("", |input, output| confirm(input, output, true));
        assert!(!closed.unwrap(), "a closed stdin never confirms");
    }

    #[test]
    fn the_review_prints_every_path_key_and_note_before_the_text() {
        let scratch = scratch_dir("review");
        let plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode, AgentTarget::Cursor],
            &[Feature::Prompt, Feature::Metrics, Feature::Land],
        )
        .unwrap();
        let (rendered, written) = with_input("", |_, output| write_review(output, &plan, true));
        rendered.unwrap();

        assert!(written.contains(".claude/CLAUDE.md"), "{written}");
        assert!(written.contains("Claude Code"), "{written}");
        assert!(written.contains("metrics: \"on\""), "{written}");
        assert!(
            written.contains("Cursor has no instruction file"),
            "the agent with nothing to write is named: {written}"
        );
        assert!(
            written.contains("<!-- pixel:setup:start -->"),
            "the exact text is shown: {written}"
        );
        let text = written.find("The exact text:").unwrap();
        let path = written.find(".claude/CLAUDE.md").unwrap();
        assert!(path < text, "the paths come before the text they belong to");
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn the_report_names_each_file_it_wrote_and_says_when_it_wrote_none() {
        let scratch = scratch_dir("report");
        let plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Prompt],
        )
        .unwrap();
        let (_, written) = with_input("", |_, output| {
            report(output, &plan, &[scratch.join(".claude/CLAUDE.md")], false)
        });
        assert!(written.contains("updated"), "{written}");
        assert!(written.contains("pixel doctor"), "{written}");

        let (_, empty) = with_input("", |_, output| report(output, &plan, &[], true));
        assert!(empty.contains("wrote nothing"), "{empty}");
        assert!(
            !empty.contains("pixel doctor"),
            "a scripted run gets no next-step advice: {empty}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    /// A scratch directory of this test's own: the crate has no `tempfile`
    /// dev-dependency, and a shared name would let two tests collide.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pixel-setup-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_defaults_are_the_catalog_defaults_and_the_markers_on_disk() {
        assert_eq!(default_features(Scope::Global).len(), 5);
        assert!(
            !default_features(Scope::Global).contains(&Feature::Land),
            "a repository-only feature is never the machine default"
        );
        assert!(
            !default_features(Scope::Repository).contains(&Feature::Land),
            "Land is opt-in even where it could be honoured"
        );

        let scratch = scratch_dir("defaults");
        // An empty directory evidences no agent, and a human drives none.
        assert!(default_agents(&scratch, Scope::Global, None).is_empty());
        std::fs::create_dir_all(scratch.join(".claude")).unwrap();
        assert_eq!(
            default_agents(&scratch, Scope::Global, None),
            vec![AgentTarget::ClaudeCode],
            "only the agent whose own marker is there is pre-checked"
        );
        std::fs::create_dir_all(scratch.join(".codex")).unwrap();
        assert_eq!(
            default_agents(&scratch, Scope::Global, None),
            vec![AgentTarget::ClaudeCode, AgentTarget::Codex],
            "a second marker adds its agent, in catalog order"
        );
        assert_eq!(
            default_agents(&scratch, Scope::Global, Some(AgentTarget::Pi)),
            vec![AgentTarget::ClaudeCode, AgentTarget::Codex, AgentTarget::Pi],
            "the harness driving the run is pre-checked even with no marker here"
        );
        assert_eq!(
            default_agents(&scratch, Scope::Repository, None),
            vec![AgentTarget::Codex],
            ".claude says nothing about the repository: Codex's repo marker is .codex"
        );
        std::fs::write(scratch.join("CLAUDE.md"), "# rules\n").unwrap();
        assert_eq!(
            default_agents(&scratch, Scope::Repository, None),
            vec![AgentTarget::ClaudeCode, AgentTarget::Codex],
            "a repository CLAUDE.md is the one unambiguous per-repository marker"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn a_config_value_renders_the_way_the_yaml_document_shows_it() {
        assert_eq!(render_value(serde_json::Value::Bool(true)), "true");
        assert_eq!(
            render_value(serde_json::Value::String("on".into())),
            "\"on\"",
            "a bare off would parse as a YAML boolean"
        );
    }
    // TEMPORARY (dev-flags): these tests go with the `dev` section — delete
    // them with it.
    mod dev_tests {
        use super::super::dev::{self, Options};
        use super::super::parse_indices as scripted;

        #[test]
        fn an_empty_answer_is_refused_rather_than_read_as_all() {
            assert!(scripted("   ", 12).is_err());
        }

        #[test]
        fn all_and_none_answer_the_list() {
            assert_eq!(scripted("all", 3).unwrap(), Some(vec![1, 2, 3]));
            assert_eq!(scripted("none", 3).unwrap(), Some(Vec::new()));
        }

        #[test]
        fn q_cancels() {
            assert_eq!(scripted("q", 3).unwrap(), None);
            assert_eq!(scripted("Q", 3).unwrap(), None);
        }

        #[test]
        fn a_list_of_indices_parses_in_either_separator() {
            assert_eq!(scripted("1,3", 4).unwrap(), Some(vec![1, 3]));
            assert_eq!(scripted("1 3", 4).unwrap(), Some(vec![1, 3]));
            assert_eq!(scripted(" 2 , 2 ,1 ", 4).unwrap(), Some(vec![2, 1]));
        }

        #[test]
        fn an_index_outside_the_list_names_the_range() {
            let err = scripted("1,9", 3).unwrap_err();
            assert!(err.contains("outside 1 to 3"), "got {err}");
            let err = scripted("0", 3).unwrap_err();
            assert!(err.contains("outside 1 to 3"), "got {err}");
            let err = scripted("two", 3).unwrap_err();
            assert!(err.contains("not a number"), "got {err}");
        }

        #[test]
        fn the_agent_flag_selects_agents_by_index() {
            let options = Options {
                selected_agents: Some("1,3".into()),
                selected_features: Some("1".into()),
                dummy_apply: false,
            };
            let answers = dev::answers(&options, pixel_install::setup::Scope::Global)
                .unwrap()
                .expect("both flags answered");
            assert_eq!(
                answers.agents,
                vec![
                    pixel_install::setup::AgentTarget::ClaudeCode,
                    pixel_install::setup::AgentTarget::Devin
                ]
            );
            assert!(!dev::scripted(&Options {
                selected_agents: Some("1".into()),
                ..Options::default()
            }));
        }

        #[test]
        fn the_feature_flag_drops_a_repository_local_feature_outside_a_repository() {
            use pixel_install::setup::{Feature, Scope};
            let land = Feature::Land.index();
            let list = format!("1,{land}");
            let global = Options {
                selected_agents: Some("1".into()),
                selected_features: Some(list.clone()),
                dummy_apply: false,
            };
            let answers = dev::answers(&global, Scope::Global).unwrap().unwrap();
            assert_eq!(
                answers.features,
                vec![Feature::Prompt],
                "a repository-only feature is dropped in a machine-wide run"
            );
            let repo = dev::answers(&global, Scope::Repository).unwrap().unwrap();
            assert_eq!(repo.features, vec![Feature::Prompt, Feature::Land]);
        }

        #[test]
        fn a_cancel_cannot_be_typed_into_a_flag() {
            let options = Options {
                selected_agents: Some("q".into()),
                selected_features: Some("q".into()),
                dummy_apply: false,
            };
            assert!(dev::answers(&options, pixel_install::setup::Scope::Global).is_err());
        }

        #[test]
        fn a_dummy_run_writes_under_the_repository_test_folder() {
            let root = dev::dummy_root(&Options {
                dummy_apply: true,
                ..Options::default()
            })
            .unwrap()
            .expect("the flag redirects the writes");
            assert!(root.ends_with("tests/setup"), "got {}", root.display());
            assert!(root.starts_with(crate::discover_root(std::path::Path::new(".")).unwrap()));
            assert_eq!(
                dev::dummy_root(&Options::default()).unwrap(),
                None,
                "a normal run resolves --repo or $HOME"
            );
            assert!(dev::skips_config(&Options {
                dummy_apply: true,
                ..Options::default()
            }));
            assert!(!dev::skips_config(&Options::default()));
        }
    }
}
