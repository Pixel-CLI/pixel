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
//! The guided setup is `pixel setup` at a terminal. The same command run
//! unattended (no terminal, `CI`, `--yes`, `--json`) is the installer in
//! `main.rs::setup_install`, unchanged. The guided run owns the hook files
//! too, through the one installer (`pixel_install::install::install`), not a
//! second writer: the review prints that installer's dry run for the agents
//! and hook groups picked, the confirmation covers it, and the apply runs the
//! same call for real.
//!
//! What is deliberately absent: repository preparation (no index, no
//! `prepare-repo`) and skill-bundle installation.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use pixel_install::install::{InstallOptions, InstallReport, InstallStep};
use pixel_install::setup::{AgentTarget, Feature, Scope, SetupPlan};

use crate::config_cmd;

/// Where a run writes, and how its answers are read.
pub struct SetupOptions {
    /// A repository to set up instead of the machine. Its root becomes the
    /// detection base and the write base.
    pub repo: Option<PathBuf>,
    /// The shell whose profile is swept for the retired `claude()` wrapper,
    /// as `pixel setup --shell` takes it.
    pub shell: Option<String>,
    /// Print the block the default answers produce and change nothing.
    pub print: bool,
    /// The development-only switches. `TEMPORARY (dev-flags)`, see `dev`.
    pub dev: dev::Options,
}

// TEMPORARY (dev-flags): the public names `main.rs` fills in and checks.
pub use dev::Options as DevOptions;

/// The refusal a released binary answers a development flag with, or `None`
/// when this build takes it. `main.rs` asks before the guided/unattended
/// decision, so a refused flag never reaches a write.
pub fn refuse_dev_flags(dev: &DevOptions) -> Option<String> {
    dev::refused(dev, dev::LIVE)
}

/// Whether `pixel setup` runs the guided setup rather than the unattended
/// installer.
///
/// Guided means a person is there to answer: stdin and stdout are terminals,
/// `CI` is not set, and neither `--yes` nor `--json` asked for no questions.
/// Both selection flags answer the questions themselves, so they run the
/// guided path without a terminal (they are development-only, see `dev`).
pub fn is_guided(options: &SetupOptions, yes: bool, json: bool) -> bool {
    use std::io::IsTerminal;
    guided_for(
        yes,
        json,
        // TEMPORARY (dev-flags): `dev::scripted` is `dev`'s; without it the
        // argument is `false`.
        dev::scripted(&options.dev),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        std::env::var_os("CI").as_deref(),
    )
}

/// The decision behind [`is_guided`], with the environment passed in.
fn guided_for(
    yes: bool,
    json: bool,
    scripted: bool,
    terminal: bool,
    ci: Option<&std::ffi::OsStr>,
) -> bool {
    let in_ci = ci.is_some_and(|value| !matches!(value.to_str(), Some("" | "0" | "false")));
    scripted || (terminal && !in_ci && !yes && !json)
}

/// Run the guided setup (or `--print`).
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
            &crate::plugin_cmd::available(),
        )
        .map_err(|e| e.to_string())?;
        print!("{}", plan.block);
        return Ok(());
    }

    // TEMPORARY (dev-flags): `scripted` is `dev`'s; without it the value is
    // `false`: `is_guided` has already required a terminal.
    let scripted = dev::scripted(&options.dev);
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

    let plan = SetupPlan::new(
        &base,
        scope,
        &answers.agents,
        &answers.features,
        &crate::plugin_cmd::available(),
    )
    .map_err(|e| e.to_string())?;
    let install = install_options(&options, &base, &plan);
    let selection = plan.install_selection();
    // The hooks are shown from a dry run of the very call that applies them,
    // so the review cannot promise a write the apply does not make.
    let hook_preview = pixel_install::install::install_selected(
        &InstallOptions {
            dry_run: true,
            ..install.clone()
        },
        Some(&selection),
    )
    .map_err(|e| e.to_string())?;
    write_review(&mut output, &plan, &hook_preview.steps, color)?;
    if !scripted && !confirm(&mut input, &mut output, color)? {
        writeln!(output, "Cancelled. Nothing was written.").map_err(io)?;
        return Ok(());
    }

    let written = plan.apply().map_err(|e| e.to_string())?;
    let hooks = pixel_install::install::install_selected(&install, Some(&selection))
        .map_err(|e| e.to_string())?;
    // TEMPORARY (dev-flags): the `if` is `dev`'s; without it the block below
    // runs unconditionally, which is what a normal run does.
    if !dev::skips_config(&options.dev) {
        config_cmd::ensure_template(options.repo.as_deref())?;
        for key in &plan.config_writes {
            config_cmd::set_global_key(key.key, key.value.to_json())?;
        }
    }
    report(&mut output, &plan, &written, &hooks, scripted)
}

/// The installer options for a plan, in the plan's scope. Under `--dummy-apply` the home directory is the dummy root
/// too, so no hook file outside it is touched.
fn install_options(options: &SetupOptions, base: &Path, plan: &SetupPlan) -> InstallOptions {
    // TEMPORARY (dev-flags): `dummy` is `dev`'s; without it the value is `false`.
    let dummy = options.dev.dummy_apply;
    InstallOptions {
        shell: options.shell.clone(),
        home: dummy.then(|| base.to_path_buf()),
        repo: (plan.scope == Scope::Repository).then(|| base.to_path_buf()),
        ..InstallOptions::default()
    }
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
        "  {} Register the hooks of the agents and features you pick",
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
fn write_review(
    output: &mut impl Write,
    plan: &SetupPlan,
    hooks: &[InstallStep],
    color: bool,
) -> Result<(), String> {
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
    if !hooks.is_empty() {
        writeln!(
            output,
            "Hooks and agent configuration (what the installer will do, from its dry run):"
        )
        .map_err(io)?;
        for step in hooks {
            writeln!(
                output,
                "  {} {}: {}",
                paint(color, "2", "•"),
                step.id,
                step.summary
            )
            .map_err(io)?;
            if let Some(detail) = &step.detail {
                writeln!(output, "      {}", paint(color, "2", detail)).map_err(io)?;
            }
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
        writeln!(output, "Not written:").map_err(io)?;
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
    hooks: &InstallReport,
    scripted: bool,
) -> Result<(), String> {
    writeln!(output).map_err(io)?;
    if written.is_empty() && plan.config_writes.is_empty() && hooks.steps.is_empty() {
        writeln!(output, "pixel setup wrote nothing.").map_err(io)?;
    } else {
        writeln!(output, "pixel setup complete:").map_err(io)?;
        for path in written {
            writeln!(output, "  updated {}", path.display()).map_err(io)?;
        }
        for step in &hooks.steps {
            writeln!(output, "  {}: {}", step.id, step.summary).map_err(io)?;
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
// They are live in a debug build only (`LIVE` below is `cfg!(debug_assertions)`):
// a released binary refuses them by name, so nothing that ships can be driven
// by them, and `--dummy-apply` can never be ignored into a write under a real
// `$HOME`. Tests and `cargo run` build with debug assertions on, which is what
// keeps the scripted cases in `tests/cli/setup_cli.rs` working.
//
// REMOVAL (before the release, tracked in docs/design/pixel-setup-plan.md and
// on issue #890). Every site is a deletion; nothing new is written:
//
//   1. delete this whole section, to the matching END marker below, and the
//      `#[cfg(test)] mod dev_tests` block in the tests module;
//   2. delete the `dev: dev::Options` field of `SetupOptions` and the
//      `refuse_dev_flags` function beside it here, and in `main.rs` the `dev`
//      argument at the construction site, the `refuse_dev_flags` call and the
//      three `--selected-agents` / `--selected-features` / `--dummy-apply`
//      clap args (all marked with the same phrase);
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

    /// Whether the development switches are live in this build.
    ///
    /// A released binary is built with debug assertions off, so the three flags
    /// are refused there: nothing that ships can be driven by them, and a
    /// silently ignored `--dummy-apply` can never become a write under a real
    /// `$HOME`. `cargo run`, `cargo test` and every other debug build accept
    /// them.
    pub(super) const LIVE: bool = cfg!(debug_assertions);

    /// The release side of [`LIVE`], enforced by the compiler rather than by a
    /// test: a build with debug assertions off fails to compile if the flags
    /// are live, so no released binary can carry them, and the release lanes in
    /// CI are the ones that prove it.
    ///
    /// A `#[cfg(not(debug_assertions))]` test would not do: the CLI integration
    /// tests link the debug binary, so such a test is compiled out of the job
    /// that runs them and never executes.
    #[cfg(not(debug_assertions))]
    const _: () = assert!(
        !LIVE,
        "the development-only setup flags must not be live in a release build"
    );

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

    /// The flags this build refuses, named with the reason, or `None` when it
    /// takes them all.
    ///
    /// `live` is a parameter rather than a read of [`LIVE`], so a test can hold
    /// both branches: the debug build that takes them and the release build
    /// that refuses them.
    pub(super) fn refused(options: &Options, live: bool) -> Option<String> {
        if live {
            return None;
        }
        let mut named: Vec<&str> = Vec::new();
        if options.selected_agents.is_some() {
            named.push("--selected-agents");
        }
        if options.selected_features.is_some() {
            named.push("--selected-features");
        }
        if options.dummy_apply {
            named.push("--dummy-apply");
        }
        if named.is_empty() {
            return None;
        }
        // One flag reads as a singular, several as a plural: naming all three
        // and then saying "is a flag" is the kind of message a reader stops
        // trusting.
        let (verb, noun, pronoun) = match named.len() {
            1 => ("is", "a development-only flag", "it"),
            _ => ("are", "development-only flags", "them"),
        };
        Some(format!(
            "{} {verb} {noun}, and this build does not have {pronoun}: \
             `pixel setup` runs the guided wizard in a terminal and the unattended \
             installer everywhere else. The flags are live in a debug build; \
             docs/design/pixel-setup-plan.md says how to test with them.",
            named.join(", ")
        ))
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
            &[
                Feature::Prompt,
                Feature::Metrics,
                Feature::Land,
                Feature::TaskHooks,
            ],
            &pixel_plugin::Available::none(),
        )
        .unwrap();
        let hooks = [step("hooks.claude", "would register the task-event hooks")];
        let (rendered, written) =
            with_input("", |_, output| write_review(output, &plan, &hooks, true));
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

    fn step(id: &str, summary: &str) -> InstallStep {
        InstallStep {
            id: id.into(),
            status: pixel_install::install::CheckStatus::Green,
            summary: summary.into(),
            detail: Some("/home/someone/.claude/settings.json".into()),
        }
    }

    fn install_report(steps: Vec<InstallStep>) -> InstallReport {
        InstallReport {
            version: "v1".into(),
            ok: true,
            executable_path: "pixel".into(),
            home: "home".into(),
            dry_run: false,
            steps,
            summary: pixel_install::install::InstallSummary {
                green: 0,
                yellow: 0,
                red: 0,
            },
        }
    }

    #[test]
    fn the_review_lists_each_hook_step_with_its_file_before_the_confirmation() {
        let scratch = scratch_dir("review-hooks");
        let plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Prompt, Feature::TaskHooks],
            &pixel_plugin::Available::none(),
        )
        .unwrap();
        let hooks = [
            step("hooks.claude", "would register the task-event hooks"),
            step("hooks.codex", "would register codex hooks"),
        ];
        let (rendered, written) =
            with_input("", |_, output| write_review(output, &plan, &hooks, false));
        rendered.unwrap();

        assert!(
            written.contains("Hooks and agent configuration"),
            "{written}"
        );
        assert!(
            written.contains("hooks.claude: would register the task-event hooks"),
            "{written}"
        );
        assert!(
            written.contains("/home/someone/.claude/settings.json"),
            "the file a hook step writes is named: {written}"
        );
        assert!(
            written.find("hooks.claude").unwrap() < written.find("The exact text:").unwrap(),
            "the hooks are part of the review, not an afterthought"
        );

        let (none, plain) = with_input("", |_, output| write_review(output, &plan, &[], false));
        none.unwrap();
        assert!(
            !plain.contains("Hooks and agent configuration"),
            "no heading over an empty list: {plain}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn the_install_options_carry_the_scope_the_shell_and_the_dummy_home() {
        let base = PathBuf::from("/tmp/pixel-setup-install-options");
        let global_plan = SetupPlan::new(
            &base,
            Scope::Global,
            &[AgentTarget::Codex],
            &[Feature::CodexConfig],
            &pixel_plugin::Available::none(),
        )
        .unwrap();
        let options = SetupOptions {
            repo: None,
            shell: Some("fish".into()),
            print: false,
            dev: DevOptions::default(),
        };
        let global = install_options(&options, &base, &global_plan);
        assert_eq!(global.shell.as_deref(), Some("fish"));
        assert_eq!(global.repo, None, "a machine-wide run is not repo-local");
        assert_eq!(global.home, None, "a real run installs under the real home");

        let repo_plan = SetupPlan::new(
            &base,
            Scope::Repository,
            &[AgentTarget::ClaudeCode],
            &[Feature::TaskHooks],
            &pixel_plugin::Available::none(),
        )
        .unwrap();
        let repo = install_options(&options, &base, &repo_plan);
        assert_eq!(repo.repo.as_deref(), Some(base.as_path()));
        assert_eq!(repo.home, None);

        let dummy = SetupOptions {
            dev: DevOptions {
                dummy_apply: true,
                ..DevOptions::default()
            },
            ..options
        };
        let redirected = install_options(&dummy, &base, &global_plan);
        assert_eq!(
            redirected.home.as_deref(),
            Some(base.as_path()),
            "--dummy-apply keeps the hook files under its own root too"
        );
    }

    #[test]
    fn the_guided_setup_needs_a_person_and_nothing_that_asks_for_no_questions() {
        let none = None;
        // A terminal, no CI, no flag: guided.
        assert!(guided_for(false, false, false, true, none));
        // No terminal on stdin or stdout: unattended.
        assert!(!guided_for(false, false, false, false, none));
        // `--yes`, `--json` and CI each turn the questions off.
        assert!(!guided_for(true, false, false, true, none));
        assert!(!guided_for(false, true, false, true, none));
        assert!(!guided_for(
            false,
            false,
            false,
            true,
            Some(std::ffi::OsStr::new("true"))
        ));
        // CI set to an empty or false-looking value is not CI.
        for off in ["", "0", "false"] {
            assert!(
                guided_for(false, false, false, true, Some(std::ffi::OsStr::new(off))),
                "CI={off:?}"
            );
        }
        // Both answers on the command line need neither a terminal nor a person.
        assert!(guided_for(false, false, true, false, none));
    }

    #[test]
    fn the_report_names_each_file_it_wrote_and_says_when_it_wrote_none() {
        let scratch = scratch_dir("report");
        let plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Prompt],
            &pixel_plugin::Available::none(),
        )
        .unwrap();
        let hooks = install_report(vec![step("hooks.claude", "registered the hooks")]);
        let (_, written) = with_input("", |_, output| {
            report(
                output,
                &plan,
                &[scratch.join(".claude/CLAUDE.md")],
                &hooks,
                false,
            )
        });
        assert!(written.contains("updated"), "{written}");
        assert!(
            written.contains("hooks.claude: registered the hooks"),
            "{written}"
        );
        assert!(written.contains("pixel doctor"), "{written}");

        let (_, empty) = with_input("", |_, output| {
            report(output, &plan, &[], &install_report(Vec::new()), true)
        });
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
        assert_eq!(default_features(Scope::Global).len(), 7);
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
        fn a_build_without_the_flags_refuses_each_one_by_name() {
            let one = Options {
                selected_agents: Some("1".into()),
                ..Options::default()
            };
            let refusal = dev::refused(&one, false).expect("refused");
            assert!(refusal.contains("--selected-agents"), "{refusal}");
            assert!(refusal.contains("is a development-only flag"), "{refusal}");

            let many = Options {
                selected_agents: Some("1".into()),
                selected_features: Some("1".into()),
                dummy_apply: true,
            };
            let refusal = dev::refused(&many, false).expect("refused");
            for flag in ["--selected-agents", "--selected-features", "--dummy-apply"] {
                assert!(refusal.contains(flag), "{flag} missing from {refusal}");
            }
            assert!(
                refusal.contains("are development-only flags"),
                "several flags read as a plural: {refusal}"
            );
            assert!(
                refusal.contains("docs/design/pixel-setup-plan.md"),
                "the refusal says where the flags are documented: {refusal}"
            );
        }

        #[test]
        fn a_build_with_the_flags_takes_them_and_an_empty_set_is_never_refused() {
            let all = Options {
                selected_agents: Some("1".into()),
                selected_features: Some("1,2".into()),
                dummy_apply: true,
            };
            assert_eq!(dev::refused(&all, true), None, "a debug build takes them");
            assert_eq!(
                dev::refused(&Options::default(), false),
                None,
                "a release build refuses nothing when nothing is asked"
            );
        }

        #[test]
        fn the_test_build_is_live_so_the_scripted_cases_keep_working() {
            // `crates/pixel/tests/cli/setup_cli.rs` drives the wizard with the
            // flags through this build. `LIVE` is `cfg!(debug_assertions)`, and
            // the test profile is a debug build: the behaviour is asserted
            // rather than the constant, so a committed `false` fails here with
            // the reason instead of somewhere in the CLI cases.
            let scripted = Options {
                selected_agents: Some("1".into()),
                selected_features: Some("1".into()),
                dummy_apply: false,
            };
            assert_eq!(
                dev::refused(&scripted, dev::LIVE),
                None,
                "this build has to take the scripted flags; `LIVE` is \
                 `cfg!(debug_assertions)`, which is true for a test profile"
            );
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
