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
use crate::prompt_key::KeyReader;

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

// TEMPORARY (dev-flags): the public name `main.rs` fills in.
pub use dev::Options as DevOptions;

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
    // One raw-mode reader for the whole run: the arrow-key lists use it, and a
    // terminal without it falls back to typing the numbers.
    let keys = KeyReader::enable();
    // Agents, features, review: the wizard's three screens.
    let mut screens = Screens {
        steps: Steps::new(3),
        color,
        keys: &keys,
    };
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stderr().lock();
    let answers = collect(
        &options,
        &base,
        scope,
        &mut screens,
        &mut input,
        &mut output,
    )?;
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
    write_review(&mut output, &plan, &hook_preview.steps, &mut screens)?;
    writeln!(output).map_err(io)?;
    writeln!(output, "Apply these changes?").map_err(io)?;
    if !scripted && !ask_confirm(&mut input, &mut output, &mut screens)? {
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
    report(&mut output, &plan, &written, &hooks, scripted, &screens)
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
    screens: &mut Screens<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<Answers>, String> {
    let running = running_agent();
    write_intro(output, base, scope, running, screens)?;
    // TEMPORARY (dev-flags): these four lines are `dev`'s; without them every
    // answer comes from the prompts below.
    if let Some(answers) = dev::answers(&options.dev, scope)? {
        return Ok(Some(answers));
    }
    let Some(agents) = ask_agents(input, output, screens, base, scope, running)? else {
        return Ok(None);
    };
    let Some(features) = ask_features(input, output, screens, scope)? else {
        return Ok(None);
    };
    Ok(Some(Answers { agents, features }))
}

// ── display ────────────────────────────────────────────────────────────────
//
// The wizard's screens follow GitButler's `but agent setup` (see
// `docs/design/but-agent-setup-reference.md`): a titled rule for the run, a
// banner per step with a right-aligned `Step N of M`, one checkbox row per
// option, and the focused row's help in dim text under the list.

/// Width of a banner rule and of the right-aligned step counter. Clamped so a
/// header reads the same on a narrow and on a very wide terminal; GitButler's
/// wizard clamps the same `40..=72`.
fn banner_width() -> usize {
    crate::select::terminal_width().clamp(40, 72)
}

/// A dim horizontal rule, the width of the banner.
fn rule(color: bool) -> String {
    paint(color, "2", &"─".repeat(banner_width()))
}

/// The columns a drawn line may use. One less than the terminal, so the last
/// column does not wrap: every line the wizard draws is counted, and the
/// arrow-key list repaints in place by moving the cursor up whole lines.
fn row_width() -> usize {
    crate::select::terminal_width().saturating_sub(1)
}

/// Split `text` into lines of at most `width` columns, on spaces, with
/// `indent` spaces before every line after the first. Every word is shown: a
/// word too long for a line is split across two, never dropped.
///
/// Wrapping rather than truncating is the point: a help line cut at the
/// terminal edge hides the part of the sentence that says what the feature
/// does, and the reader has no way to ask for the rest.
fn wrap(text: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.max(indent + 1);
    let mut lines: Vec<String> = Vec::new();
    // The line being built, without its indent: the indent is added when the
    // line is closed, so `current.is_empty()` keeps meaning "no word yet".
    let mut current = String::new();
    // The columns used so far on the line being built.
    let used = |lines: &[String], current: &str| {
        (if lines.is_empty() { 0 } else { indent }) + current.chars().count()
    };

    for word in text.split_whitespace() {
        let mut rest = word;
        // Bounded by `rest`, which loses at least one character per turn: a
        // `loop` here would spin on the empty remainder and allocate until the
        // process is killed (the SIGKILL that started this).
        while !rest.is_empty() {
            let used_now = used(&lines, &current);
            let separator = usize::from(!current.is_empty());
            let room = width.saturating_sub(used_now + separator);

            // A whole word goes on the line it fits on, even if that is the
            // next one: splitting it when the line below has room is how
            // "three" became "th" + "ree".
            if rest.chars().count() <= room {
                if separator == 1 {
                    current.push(' ');
                }
                current.push_str(rest);
                rest = "";
                continue;
            }
            if separator == 1 {
                close_line(&mut lines, &mut current, indent);
                continue;
            }
            // A line of its own and still too long: split it, losing nothing.
            if room == 0 {
                close_line(&mut lines, &mut current, indent);
                continue;
            }
            let split = rest
                .char_indices()
                .nth(room)
                .map_or(rest.len(), |(index, _)| index);
            current.push_str(&rest[..split]);
            rest = &rest[split..];
            if !rest.is_empty() {
                close_line(&mut lines, &mut current, indent);
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        close_line(&mut lines, &mut current, indent);
    }
    lines
}

/// Finish the line being built: the indent belongs to every line but the first.
fn close_line(lines: &mut Vec<String>, current: &mut String, indent: usize) {
    let text = std::mem::take(current);
    if lines.is_empty() {
        lines.push(text);
    } else {
        lines.push(format!("{}{text}", " ".repeat(indent)));
    }
}

/// The wizard's position, so every screen says which step it is.
struct Steps {
    current: usize,
    total: usize,
}

impl Steps {
    fn new(total: usize) -> Self {
        Self { current: 0, total }
    }

    /// The step's banner: its title on the left, `Step N of M` on the right,
    /// and a rule under both.
    fn banner(&mut self, output: &mut impl Write, title: &str, color: bool) -> Result<(), String> {
        self.current += 1;
        let right = format!("Step {} of {}", self.current, self.total);
        let gap = banner_width()
            .saturating_sub(title.chars().count() + right.chars().count())
            .max(1);
        writeln!(output).map_err(io)?;
        writeln!(
            output,
            "{}{}{}",
            paint(color, "1", title),
            " ".repeat(gap),
            paint(color, "2", &right)
        )
        .map_err(io)?;
        writeln!(output, "{}", rule(color)).map_err(io)?;
        Ok(())
    }
}

/// The state every screen shares: the wizard's position, whether colour is on,
/// and which key mode this run has. One value, so a question takes the answers
/// rather than a fistful of terminal details.
struct Screens<'a> {
    steps: Steps,
    color: bool,
    keys: &'a KeyReader,
}

fn write_intro(
    output: &mut impl Write,
    base: &Path,
    scope: Scope,
    detected: Option<AgentTarget>,
    screens: &Screens<'_>,
) -> Result<(), String> {
    let color = screens.color;
    let keys = screens.keys;
    writeln!(output).map_err(io)?;
    writeln!(output, "{}", paint(color, "1;32", "pixel · setup")).map_err(io)?;
    writeln!(output, "{}", rule(color)).map_err(io)?;
    writeln!(output, "Set up your coding agent to work well with pixel.").map_err(io)?;
    writeln!(output).map_err(io)?;
    writeln!(output, "Here's what this does:").map_err(io)?;
    for line in [
        "Write a managed pixel block into each agent's instruction file",
        "Register the hooks of the agents and features you pick",
        "Store the settings the features you pick need",
    ] {
        writeln!(output, "  {} {line}", paint(color, "2", "•")).map_err(io)?;
    }
    writeln!(output).map_err(io)?;
    writeln!(
        output,
        "{}",
        paint(
            color,
            "2",
            "Nothing is written until you review and confirm — you'll see exactly what changes first."
        )
    )
    .map_err(io)?;
    writeln!(output, "{}", paint(color, "2", &list_keys_hint(keys))).map_err(io)?;
    let where_ = match scope {
        Scope::Global => "this machine ($HOME)",
        Scope::Repository => "this repository",
    };
    let driving = detected.map_or(String::new(), |agent| {
        format!("  ·  driving: {}", agent.name())
    });
    for line in help_lines(
        &format!("Writes under {}", base.display()),
        row_width(),
        color,
    ) {
        writeln!(output, "{line}").map_err(io)?;
    }
    writeln!(
        output,
        "{}",
        paint(color, "2", &format!("{where_}{driving}"))
    )
    .map_err(io)?;
    Ok(())
}

/// The key hints the list questions share, for the input mode this run has.
fn list_keys_hint(keys: &KeyReader) -> String {
    if keys.active() {
        "↑/↓ move · space select · a all · n none · enter continue · esc cancel".to_string()
    } else {
        "Type numbers (1,3), `all` or `none` · enter keeps the shown defaults · q cancels"
            .to_string()
    }
}

/// The agent question: a list of harnesses, pre-checked from detection.
fn ask_agents(
    input: &mut impl BufRead,
    output: &mut impl Write,
    screens: &mut Screens<'_>,
    base: &Path,
    scope: Scope,
    running: Option<AgentTarget>,
) -> Result<Option<Vec<AgentTarget>>, String> {
    let defaults = default_agents(base, scope, running);
    let rows: Vec<Row> = AgentTarget::ALL
        .iter()
        .map(|agent| {
            let note = if Some(*agent) == running {
                " (driving now)"
            } else if defaults.contains(agent) {
                " (detected)"
            } else {
                ""
            };
            Row {
                label: format!("{}. {}{note}", agent.index(), agent.name()),
                help: agent.help(),
                disabled: false,
            }
        })
        .collect();
    let picked = ask_list(
        input,
        output,
        screens,
        "Agents",
        "Which agents should pixel write for?",
        &rows,
        &indices(&defaults, |agent| agent.index()),
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

/// The feature question: a list of pixel features, pre-checked from the
/// catalog defaults this scope can honour.
fn ask_features(
    input: &mut impl BufRead,
    output: &mut impl Write,
    screens: &mut Screens<'_>,
    scope: Scope,
) -> Result<Option<Vec<Feature>>, String> {
    let defaults = default_features(scope);
    let rows: Vec<Row> = Feature::ALL
        .iter()
        .map(|feature| {
            let disabled = feature.repo_local_only() && scope != Scope::Repository;
            Row {
                label: format!("{}. {}", feature.index(), feature.label()),
                help: if disabled {
                    feature.repo_local_help()
                } else {
                    feature.help()
                },
                disabled,
            }
        })
        .collect();
    let picked = ask_list(
        input,
        output,
        screens,
        "Features",
        "Which features should be on?",
        &rows,
        &indices(&defaults, |feature| feature.index()),
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

/// One selectable row: what it says, the help shown while the cursor is on it,
/// and whether this scope can use it at all.
struct Row {
    label: String,
    help: &'static str,
    disabled: bool,
}

/// A row's cursor and checkbox, the columns a wrapped label lines up under.
fn row_prefix(cursor: bool, checked: bool) -> String {
    let pointer = if cursor { ">" } else { " " };
    let checkbox = if checked { "[x]" } else { "[ ]" };
    format!("{pointer} {checkbox} ")
}

/// A row's label, with the note a row this scope cannot use carries. The long
/// reason is the help line under the list.
fn row_label(row: &Row) -> String {
    if row.disabled {
        format!("{} (not in this scope)", row.label)
    } else {
        row.label.clone()
    }
}

/// A row as drawn: the checkbox on the first line, the label wrapped under it,
/// then styled. A row this scope cannot use is dim; the row under the cursor is
/// bold, its checkbox green.
///
/// The label is wrapped *before* the prefix is added: `wrap` collapses runs of
/// whitespace, so wrapping the prefixed line would eat the two columns the
/// cursor and the checkbox occupy and pull every row to the left edge.
fn row_lines(row: &Row, cursor: bool, checked: bool, width: usize, color: bool) -> Vec<String> {
    let prefix = row_prefix(cursor, checked);
    let room = width.saturating_sub(prefix.chars().count());
    let mut lines = wrap(&row_label(row), room.max(1), prefix.chars().count());
    let first = lines.first_mut().expect("wrap always returns one line");
    *first = format!("{prefix}{first}");

    lines
        .into_iter()
        .map(|line| {
            if row.disabled {
                return paint(color, "2", &line);
            }
            let cut = line
                .char_indices()
                .nth(prefix.chars().count())
                .map_or(line.len(), |(at, _)| at);
            let (box_, label) = line.split_at(cut);
            let box_ = if checked {
                paint(color, "1;32", box_)
            } else {
                box_.to_string()
            };
            let label = if cursor {
                paint(color, "1", label)
            } else {
                label.to_string()
            };
            format!("{box_}{label}")
        })
        .collect()
}

/// One line of dim help, wrapped, with its two-space indent.
fn help_lines(text: &str, width: usize, color: bool) -> Vec<String> {
    wrap(text, width.saturating_sub(2), 2)
        .into_iter()
        .map(|line| format!("  {}", paint(color, "2", &line)))
        .collect()
}

/// A numbered question over `rows`. On a live terminal the arrows move, space
/// toggles and enter answers; otherwise the answer is typed as indices. An
/// empty line keeps `default`, `all` and `none` are accepted, `q` cancels.
fn ask_list(
    input: &mut impl BufRead,
    output: &mut impl Write,
    screens: &mut Screens<'_>,
    title: &str,
    question: &str,
    rows: &[Row],
    default: &[usize],
) -> Result<Option<Vec<usize>>, String> {
    let color = screens.color;
    screens.steps.banner(output, title, color)?;
    if screens.keys.active()
        && let Some(mut raw) = crate::prompt_key::RawGuard::new()
    {
        writeln!(output, "{question}").map_err(io)?;
        return ask_list_keys(output, rows, default, &mut raw, color);
    }
    ask_list_lines(input, output, question, rows, default, color)
}

/// The arrow-key list: the same rows, repainted in place as the cursor and the
/// checkboxes move, with the focused row's help under them.
#[cfg_attr(test, mutants::skip)] // the raw branch needs a real tty; the key map and the state machine are tested pure in `prompt_key`, the line branch below
fn ask_list_keys(
    output: &mut impl Write,
    rows: &[Row],
    default: &[usize],
    raw: &mut crate::prompt_key::RawGuard,
    color: bool,
) -> Result<Option<Vec<usize>>, String> {
    use crate::prompt_key::{ListOutcome, ListState};

    let disabled: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.disabled)
        .map(|(index, _)| index + 1)
        .collect();
    let mut state = ListState::new(rows.len(), default);
    let mut drawn = 0usize;
    loop {
        // Rewind over the block drawn last time (the rows and the help line)
        // and paint it again: every line is one terminal row, because `fit`
        // truncated it, so the count is exact.
        if drawn > 0 {
            write!(output, "\x1b[{drawn}A").map_err(io)?;
        }
        let width = row_width();
        let mut block: Vec<String> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            block.extend(row_lines(
                row,
                index == state.cursor,
                state.checked[index],
                width,
                color,
            ));
        }
        let help = rows.get(state.cursor).map_or("", |row| row.help);
        block.extend(help_lines(help, width, color));
        for line in &block {
            writeln!(output, "{line}").map_err(io)?;
        }
        // Every drawn line is counted: the rewind below moves by whole lines.
        drawn = block.len();
        output.flush().map_err(io)?;

        match state.apply(raw.read_list_key(), &disabled) {
            ListOutcome::Redraw => {}
            ListOutcome::Confirm => return Ok(Some(state.picked())),
            ListOutcome::Cancel => return Ok(None),
            ListOutcome::Ignored => {
                // Nothing changed, so nothing is repainted: rewind anyway so
                // the next key does not paint one line too low.
                write!(output, "\x1b[{drawn}A").map_err(io)?;
                drawn = 0;
            }
        }
    }
}

/// The line-driven list: the rows, then the answer as numbers.
fn ask_list_lines(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
    rows: &[Row],
    default: &[usize],
    color: bool,
) -> Result<Option<Vec<usize>>, String> {
    let total = rows.len();
    // The typed answer replaces the selection, so nothing here toggles: the
    // checkboxes only show which rows the default already has on.
    let checked: Vec<bool> = (1..=total).map(|index| default.contains(&index)).collect();
    loop {
        // The question is repeated with the list: an answer the reader could
        // not parse re-asks here, and a screen that had scrolled away leaves
        // the reader the question to answer.
        writeln!(output, "{question}").map_err(io)?;
        let width = row_width();
        for (index, row) in rows.iter().enumerate() {
            for line in row_lines(row, false, checked[index], width, color) {
                writeln!(output, "{line}").map_err(io)?;
            }
        }
        // The typed path has no cursor, so the focused row's help is never
        // shown: a row this scope cannot use has to carry its reason here, or
        // the reader never learns how to enable it.
        for row in rows.iter().filter(|row| row.disabled) {
            for line in help_lines(row.help, width, color) {
                writeln!(output, "{line}").map_err(io)?;
            }
        }
        write!(
            output,
            "{} > ",
            paint(
                color,
                "1",
                &format!(
                    "Your answer [{total} numbers, `all`, `none`] default [{}]",
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
                writeln!(output).map_err(io)?;
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
    screens: &mut Screens<'_>,
) -> Result<(), String> {
    let color = screens.color;
    screens.steps.banner(output, "Review", color)?;
    writeln!(
        output,
        "pixel setup will write under {}.",
        paint(color, "1", &plan.base.display().to_string())
    )
    .map_err(io)?;
    if !plan.instruction_writes.is_empty() {
        writeln!(output, "{}", paint(color, "1", "Instruction files")).map_err(io)?;
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
        writeln!(output, "{}", paint(color, "1", "Rule files")).map_err(io)?;
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
            "{}",
            paint(
                color,
                "1",
                "Hooks and agent configuration (from the installer's dry run)"
            )
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
        writeln!(
            output,
            "{}",
            paint(color, "1", "Global settings (~/.pixel/config.yaml)")
        )
        .map_err(io)?;
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

/// The question between the review and the writes: an Apply/Cancel list on a
/// live terminal, a `[Y/n]` line everywhere else.
fn ask_confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    screens: &mut Screens<'_>,
) -> Result<bool, String> {
    let color = screens.color;
    if screens.keys.active()
        && let Some(mut raw) = crate::prompt_key::RawGuard::new()
    {
        return Ok(ask_confirm_keys(output, &mut raw, color)?.unwrap_or(false));
    }
    confirm(input, output, color)
}

/// The Apply/Cancel list: the cursor moves, enter answers, esc cancels.
#[cfg_attr(test, mutants::skip)] // the raw branch needs a real tty; the line branch below is what the tests drive
fn ask_confirm_keys(
    output: &mut impl Write,
    raw: &mut crate::prompt_key::RawGuard,
    color: bool,
) -> Result<Option<bool>, String> {
    use crate::prompt_key::ListKey;

    const CHOICES: [(&str, &str); 2] = [
        (
            "Apply",
            "Register the hooks, install the skills and write the files now.",
        ),
        ("Cancel", "Change nothing and exit."),
    ];
    let mut picked = 0usize;
    let mut drawn = 0usize;
    loop {
        // Rewind over the block drawn last time, then paint it again: one
        // terminal row per line, because `fit` truncated it.
        if drawn > 0 {
            write!(output, "\x1b[{drawn}A").map_err(io)?;
        }
        let width = row_width();
        let mut block: Vec<String> = Vec::new();
        for (index, (label, _)) in CHOICES.iter().enumerate() {
            let line = if index == picked {
                format!("> {}", paint(color, "1", label))
            } else {
                format!("  {label}")
            };
            block.push(line);
        }
        block.extend(help_lines(CHOICES[picked].1, width, color));
        for line in &block {
            writeln!(output, "{line}").map_err(io)?;
        }
        drawn = block.len();
        output.flush().map_err(io)?;

        match raw.read_list_key() {
            ListKey::Up | ListKey::Down => picked = 1 - picked,
            ListKey::Enter => return Ok(Some(picked == 0)),
            ListKey::Cancel => return Ok(None),
            // `n` reads as "no" to this question, and doing nothing on it
            // would look like a dropped keypress.
            ListKey::None => return Ok(Some(false)),
            // Space and `a`: nothing this question uses, so nothing is
            // repainted; the rewind leaves the next key writing in place.
            ListKey::Space | ListKey::All => {
                write!(output, "\x1b[{drawn}A").map_err(io)?;
                drawn = 0;
            }
        }
    }
}

/// The line-driven confirmation: `[Y/n]`.
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
    screens: &Screens<'_>,
) -> Result<(), String> {
    let color = screens.color;
    writeln!(output).map_err(io)?;
    writeln!(output, "{}", rule(color)).map_err(io)?;
    if written.is_empty() && plan.config_writes.is_empty() && hooks.steps.is_empty() {
        writeln!(output, "pixel setup wrote nothing.").map_err(io)?;
    } else {
        writeln!(output, "{}", paint(color, "1;32", "pixel setup complete")).map_err(io)?;
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

    /// Rows for a test list.
    fn rows(labels: &[&str]) -> Vec<Row> {
        labels
            .iter()
            .map(|label| Row {
                label: (*label).to_string(),
                help: "the help line",
                disabled: false,
            })
            .collect()
    }

    /// The shared screen state of a test run, with the typed input mode:
    /// `KeyReader::inert` keeps the path a test can drive. Colour is off, so an
    /// assertion reads the text a reader sees rather than the SGR between it.
    fn screens(keys: &KeyReader) -> Screens<'_> {
        Screens {
            steps: Steps::new(3),
            color: false,
            keys,
        }
    }

    /// One line-driven question.
    fn ask(
        input: &mut std::io::Cursor<Vec<u8>>,
        output: &mut Vec<u8>,
        rows: &[Row],
        default: &[usize],
    ) -> Result<Option<Vec<usize>>, String> {
        let keys = KeyReader::inert();
        ask_list(
            input,
            output,
            &mut screens(&keys),
            "Pick",
            "Pick one:",
            rows,
            default,
        )
    }

    #[test]
    fn the_list_screen_is_a_step_banner_over_checkbox_rows() {
        let (_, written) = with_input("\n", |input, output| {
            ask(input, output, &rows(&["a", "b", "c"]), &[1, 3])
        });
        let banner = written.find("Pick").expect("the step title");
        let rule = written.find('─').expect("a rule under the banner");
        let question = written.find("Pick one:").expect("the question");
        assert!(banner < rule && rule < question, "got {written}");
        assert!(written.contains("Step 1 of 3"), "got {written}");
        assert!(written.contains("  [x] a"), "got {written}");
        assert!(written.contains("  [ ] b"), "got {written}");
        assert!(written.contains("  [x] c"), "got {written}");
        assert!(
            written.contains("Your answer [3 numbers, `all`, `none`] default [1,3]"),
            "the answer line names the default it will keep: {written}"
        );
    }

    #[test]
    fn a_disabled_row_is_marked_and_its_reason_is_printed_below() {
        let mut rows = rows(&["usable", "land"]);
        rows[1].disabled = true;
        rows[1].help = "Re-run with --repo <path> to land work on the target branch.";
        let (_, written) = with_input("\n", |input, output| ask(input, output, &rows, &[1]));
        assert!(
            written.contains("[ ] land (not in this scope)"),
            "the row says what it is: {written}"
        );
        assert!(
            written.contains("Re-run with --repo <path>"),
            "and the reason follows, because a typed answer has no cursor to move onto it: {written}"
        );
    }

    #[test]
    fn the_list_prompt_asks_again_after_an_answer_it_cannot_read() {
        let (picked, written) = with_input("nonsense\n2\n", |input, output| {
            ask(input, output, &rows(&["a", "b"]), &[1])
        });
        assert_eq!(picked.unwrap(), Some(vec![2]));
        assert!(
            written.contains("not a number"),
            "the reader has to say what was wrong: {written}"
        );
        assert_eq!(
            written.matches("Pick one:").count(),
            2,
            "the list is shown again: {written}"
        );
    }

    #[test]
    fn the_list_prompt_keeps_the_shown_default_on_an_empty_line() {
        let (picked, _) = with_input("\n", |input, output| {
            ask(input, output, &rows(&["a", "b"]), &[1, 2])
        });
        assert_eq!(picked.unwrap(), Some(vec![1, 2]));
    }

    #[test]
    fn the_list_prompt_cancels_on_q_and_on_end_of_input() {
        let (cancelled, _) = with_input("q\n", |input, output| {
            ask(input, output, &rows(&["a"]), &[1])
        });
        assert_eq!(cancelled.unwrap(), None);
        let (ended, _) = with_input("", |input, output| ask(input, output, &rows(&["a"]), &[1]));
        assert_eq!(ended.unwrap(), None, "a closed stdin is not an answer");
    }

    #[test]
    fn a_long_line_wraps_on_spaces_and_every_word_is_shown() {
        assert_eq!(wrap("short", 40, 0), vec!["short"]);
        assert_eq!(
            wrap("one two three four", 10, 0),
            vec!["one two", "three four"],
            "the break lands between words"
        );
        assert_eq!(
            wrap("one two three", 10, 3),
            vec!["one two", "   three"],
            "a continuation line lines up under the indent"
        );
    }

    #[test]
    fn a_wrapped_word_is_split_rather_than_dropped() {
        let lines = wrap(&"x".repeat(25), 10, 0);
        assert_eq!(
            lines.concat().len(),
            25,
            "not one character of a long word may be lost: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| line.chars().count() <= 10),
            "{lines:?}"
        );
    }

    #[test]
    fn every_wrapped_line_fits_the_width_it_was_given() {
        let help = "Prefer `pixel search-meaning` for a question about intent, and \
                    `pixel search-content` when you already have the text.";
        for width in [20usize, 41, 72] {
            let lines = wrap(help, width, 2);
            assert!(
                lines.iter().all(|line| line.chars().count() <= width),
                "width {width}: {lines:?}"
            );
            assert_eq!(
                lines.join(" ").split_whitespace().count(),
                help.split_whitespace().count(),
                "width {width} dropped a word"
            );
        }
    }

    #[test]
    fn an_empty_help_is_one_empty_line_so_the_repaint_count_stays_exact() {
        assert_eq!(wrap("", 40, 0), vec![String::new()]);
    }

    #[test]
    fn a_row_wraps_its_own_label_and_the_continuation_lines_up_under_it() {
        let row = Row {
            label: "12. Land on the target branch, and the rest of a long label".into(),
            help: "help",
            disabled: false,
        };
        let lines = row_lines(&row, true, true, 24, false);
        assert!(lines.len() > 1, "a label past the width wraps: {lines:?}");
        assert!(lines[0].starts_with("> [x] "), "{lines:?}");
        assert_eq!(
            lines[1].chars().take_while(|c| *c == ' ').count(),
            row_prefix(true, true).chars().count(),
            "the continuation lines up under the label: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| line.chars().count() <= 24),
            "{lines:?}"
        );
    }

    #[test]
    fn with_colour_on_the_checkbox_is_green_and_the_cursor_row_is_bold() {
        let row = Row {
            label: "3. Cursor".into(),
            help: "help",
            disabled: false,
        };
        let on = row_lines(&row, true, true, 40, true);
        assert!(on[0].contains("\x1b[1;32m"), "the checkbox: {on:?}");
        assert!(
            on[0].ends_with("\x1b[0m") || on[0].contains("\x1b[1m"),
            "the label carries the cursor: {on:?}"
        );
        let plain = row_lines(&row, true, true, 40, false);
        assert_eq!(plain, vec!["> [x] 3. Cursor"], "no SGR when colour is off");
    }

    #[test]
    fn a_disabled_row_is_dim_and_keeps_its_reason_out_of_the_label() {
        let row = Row {
            label: "12. Land".into(),
            help: "why",
            disabled: true,
        };
        let lines = row_lines(&row, false, false, 40, false);
        assert_eq!(lines, vec!["  [ ] 12. Land (not in this scope)"]);
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
        let keys = KeyReader::inert();
        let (rendered, written) = with_input("", |_, output| {
            write_review(output, &plan, &hooks, &mut screens(&keys))
        });
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
        let keys = KeyReader::inert();
        let (rendered, written) = with_input("", |_, output| {
            write_review(output, &plan, &hooks, &mut screens(&keys))
        });
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

        let (none, plain) = with_input("", |_, output| {
            write_review(output, &plan, &[], &mut screens(&keys))
        });
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
        let keys = KeyReader::inert();
        let (_, written) = with_input("", |_, output| {
            report(
                output,
                &plan,
                &[scratch.join(".claude/CLAUDE.md")],
                &hooks,
                false,
                &screens(&keys),
            )
        });
        assert!(written.contains("updated"), "{written}");
        assert!(
            written.contains("hooks.claude: registered the hooks"),
            "{written}"
        );
        assert!(written.contains("pixel doctor"), "{written}");

        let (_, empty) = with_input("", |_, output| {
            report(
                output,
                &plan,
                &[],
                &install_report(Vec::new()),
                true,
                &screens(&keys),
            )
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
