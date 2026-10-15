// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: the interactive per-agent, per-feature onboarding wizard.
//!
//! Ported from GitButler's `but agent setup` (read at `5cbe33d`). The planning
//! half lives in [`pixel_setup::setup`], which knows the agent targets, the
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
//! too, through the one installer (`pixel_setup::install::install`), not a
//! second writer: the review prints that installer's dry run for the agents
//! and hook groups picked, the confirmation covers it, and the apply runs the
//! same call for real.
//!
//! What is deliberately absent: repository preparation (no index, no
//! `prepare-repo`) and skill-bundle installation.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use pixel_setup::install::{InstallOptions, InstallReport, InstallStep};
use pixel_setup::setup::{AgentTarget, Feature, Scope, SetupPlan};

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
        // The block is retired: what `pixel setup` deploys is the skill.
        print!("{}", pixel_setup::skill::PIXEL_SKILL);
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
    // On a live terminal the wizard takes the alternate screen (#863): every
    // card is drawn there and the shell comes back clean when it ends.
    let alt_screen = !scripted && std::io::stderr().is_terminal() && keys.active();
    if alt_screen {
        write!(output, "\x1b[?1049h").map_err(io)?;
    }
    let result = run_questions(
        &options,
        base,
        scope,
        scripted,
        &mut screens,
        &mut input,
        &mut output,
        color,
    );
    if alt_screen {
        let _ = write!(output, "\x1b[?1049l");
    }
    result
}

/// The wizard's body: collect the answers, review, confirm, apply, report,
/// then the classify journey when it was picked.
#[allow(clippy::too_many_arguments)] // the pieces come from `run`'s setup
fn run_questions(
    options: &SetupOptions,
    base: PathBuf,
    scope: Scope,
    scripted: bool,
    screens: &mut Screens<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
    color: bool,
) -> Result<(), String> {
    let answers = collect(options, &base, scope, screens, input, output)?;
    let Some(answers) = answers else {
        writeln!(output).map_err(io)?;
        writeln!(
            output,
            "{}",
            paint(
                color,
                "2",
                "Cancelled. Nothing was written and no setting was changed."
            )
        )
        .map_err(io)?;
        return Ok(());
    };

    // A picked AI classification adds its own screen after the apply — the
    // engine menu and helpers — so the run has a fourth step to count to.
    if answers.features.contains(&Feature::Classify) {
        screens.steps.total += 1;
    }
    let plan = SetupPlan::new(&base, scope, &answers.agents, &answers.features)
        .map_err(|e| e.to_string())?;
    let install = install_options(options, &base, &plan);
    let selection = plan.install_selection();
    // The hooks are shown from a dry run of the very call that applies them,
    // so the review cannot promise a write the apply does not make.
    let hook_preview = pixel_setup::install::install_selected(
        &InstallOptions {
            dry_run: true,
            ..install.clone()
        },
        Some(&selection),
    )
    .map_err(|e| e.to_string())?;
    write_review(output, &plan, &hook_preview.steps, screens)?;
    writeln!(output).map_err(io)?;
    writeln!(output, "Apply these changes?").map_err(io)?;
    if !scripted && !ask_confirm(input, output, screens)? {
        writeln!(
            output,
            "{}",
            paint(color, "2", "Cancelled. Nothing was written.")
        )
        .map_err(io)?;
        return Ok(());
    }

    let hooks = pixel_setup::install::install_selected(&install, Some(&selection))
        .map_err(|e| e.to_string())?;
    // TEMPORARY (dev-flags): the `if` is `dev`'s; without it the block below
    // runs unconditionally, which is what a normal run does.
    if !dev::skips_config(&options.dev) {
        config_cmd::ensure_template(options.repo.as_deref())?;
        for key in &plan.config_writes {
            config_cmd::set_global_key(key.key, key.value.to_json())?;
        }
    }
    report(output, &plan, &hooks, scripted, screens)?;
    // Picking AI classification opens its own journey after the files land:
    // the engine menu (local Ollaya, remote preset, OpenAI Decisions, Jev,
    // Clef-flash) and the optional harness helpers. A scripted run gets the
    // printed suggestion instead of a second set of questions.
    if answers.features.contains(&Feature::Classify) {
        if !scripted {
            screens.steps.banner(output, "Classify", color)?;
        }
        crate::classify_setup::install_step(!scripted, input, output)?;
    }
    Ok(())
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
/// [`pixel_setup::setup::detect`] inside: detection reads the process
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
    pixel_setup::setup::detect().map(AgentTarget::from_detected)
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
// `docs/archive/design/but-agent-setup-reference.md`): a titled rule for the run, a
// banner per step with a right-aligned `Step N of M`, one checkbox row per
// option, and the focused row's help in dim text under the list.

/// Width of a banner rule and of the right-aligned step counter. Clamped so a
/// header reads the same on a narrow and on a very wide terminal; the card
/// border's two columns come off the top, so the box never exceeds the screen.
fn banner_width() -> usize {
    // The card's border adds two columns, so the width is clamped under the
    // terminal's: a box wider than the screen wraps and its rows stop being
    // one terminal line each, which the repaints count on.
    crate::select::terminal_width()
        .saturating_sub(2)
        .clamp(24, 72)
}

/// A dim horizontal rule, the width of the banner.
fn rule(color: bool) -> String {
    paint(color, "2", &"─".repeat(banner_width()))
}

/// One line of a rounded `╭─╮` box: `content` is padded to `width` columns of
/// visible text so the right border stays square with colour on or off.
fn box_row(color: bool, content: &str, width: usize) -> String {
    let visible = crate::select::visible_len(content);
    let pad = width.saturating_sub(2 + visible);
    let bar = paint(color, "2", "│");
    format!("{bar} {content}{} {bar}", " ".repeat(pad))
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

    /// The step's banner: a brand dot and title on the left, progress dots and
    /// `Step N of M` dim on the right, and a rule under both.
    fn banner(&mut self, output: &mut impl Write, title: &str, color: bool) -> Result<(), String> {
        self.current += 1;
        // The dots are the progress the issue asks for: filled up to the
        // current step, hollow past it, beside the written count.
        let dots: String = (1..=self.total)
            .map(|step| {
                if step <= self.current {
                    paint(color, "92", "●")
                } else {
                    paint(color, "2", "○")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let right = format!("{}  Step {} of {}", dots, self.current, self.total);
        let title_visible = format!("● {title}");
        let title_rendered = format!("{} {}", paint(color, "92", "●"), paint(color, "1", title));
        let gap = banner_gap(&title_visible, &right, banner_width());
        writeln!(output).map_err(io)?;
        writeln!(
            output,
            "{}{}{}",
            title_rendered,
            " ".repeat(gap),
            paint(color, "2", &right)
        )
        .map_err(io)?;
        writeln!(output, "{}", rule(color)).map_err(io)?;
        Ok(())
    }
}

/// The spaces between a step's title and its right-aligned counter: at least
/// one, so a title longer than the banner never runs into the counter. Widths
/// are counted visibly so SGR sequences do not skew the alignment.
fn banner_gap(title: &str, right: &str, width: usize) -> usize {
    width
        .saturating_sub(crate::select::visible_len(title) + crate::select::visible_len(right))
        .max(1)
}

/// Move the cursor back over the `drawn` lines last written and erase them.
///
/// Rewinding alone only overwrites: a new line shorter than the old one — a
/// help sentence that now wraps onto fewer lines, a label that got shorter —
/// leaves the old tail visible after it, and a block with fewer lines leaves
/// its last line under the list. `\r` goes to the start of the line and
/// `ESC [ J` erases from there to the end of the screen, which is the block: it
/// is the last thing the wizard draws.
fn rewind_block(output: &mut impl Write, drawn: usize) -> Result<(), String> {
    write!(output, "\x1b[{drawn}A\r\x1b[J").map_err(io)
}

/// The state every screen shares: the wizard's position, whether colour is on,
/// and which key mode this run has. One value, so a question takes the answers
/// rather than a fistful of terminal details.
struct Screens<'a> {
    steps: Steps,
    color: bool,
    keys: &'a KeyReader,
}

/// The intro card's lines: the logo as the header when it fits (the short
/// pulse the issue asks for is drawn over it), the `pixel · setup` wordmark
/// otherwise, then what the wizard does and the nothing-until-confirm
/// promise. `logo_code` is the SGR the logo rows are painted with, so the
/// pulse can draw the same card dim and then bright.
fn intro_card(width: usize, color: bool, logo_code: &str, show_logo: bool) -> Vec<String> {
    let inner = width.saturating_sub(2);
    let mut card = vec![format!("╭{}╮", "─".repeat(width))];
    if show_logo {
        card.push(box_row(color, "", width));
        for line in pixel_setup::banner::logo_lines() {
            card.push(box_row(color, &paint(color, logo_code, line), width));
        }
        card.push(box_row(
            color,
            &format!(
                "{} {}",
                paint(color, "2", env!("CARGO_PKG_VERSION")),
                paint(color, "2", "· setup")
            ),
            width,
        ));
    } else {
        card.push(box_row(
            color,
            &format!(
                "{} {}",
                paint(color, "92", "●"),
                paint(color, "1", "pixel · setup")
            ),
            width,
        ));
    }
    card.push(box_row(color, "", width));
    for line in wrap(
        "Set up your coding agent to work well with pixel.",
        inner.saturating_sub(2),
        0,
    ) {
        card.push(box_row(color, &paint(color, "2", &line), width));
    }
    card.push(box_row(color, "", width));
    card.push(box_row(
        color,
        &paint(color, "1", "Here's what this does:"),
        width,
    ));
    for bullet in [
        "Install the pixel skill into each picked harness's skills dir",
        "Register the hooks of the agents and features you pick",
        "Store the settings the features you pick need",
    ] {
        card.push(box_row(
            color,
            &format!("{} {}", paint(color, "92", "✦"), bullet),
            width,
        ));
    }
    card.push(box_row(color, "", width));
    for line in wrap(
        "Nothing is written until you review and confirm — you'll see exactly what changes first.",
        inner.saturating_sub(2),
        0,
    ) {
        card.push(box_row(color, &paint(color, "2", &line), width));
    }
    card.push(format!("╰{}╯", "─".repeat(width)));
    card
}

/// The length of the logo's dimming pulse: one beat, not the retired
/// multi-second animation the issue replaced with this card.
const LOGO_PULSE: std::time::Duration = std::time::Duration::from_millis(160);

fn write_intro(
    output: &mut impl Write,
    base: &Path,
    scope: Scope,
    detected: Option<AgentTarget>,
    screens: &Screens<'_>,
) -> Result<(), String> {
    let color = screens.color;
    let keys = screens.keys;
    let width = banner_width();
    // The logo needs its 50 columns plus the card's padding; a narrower
    // terminal gets the `● pixel · setup` wordmark instead of a clipped mark.
    let show_logo = pixel_setup::banner::logo_width() + 4 <= width;
    // The pulse is the card header the issue asks for: on a live terminal the
    // logo lands dim and brightens once; elsewhere the card is drawn bright
    // the one time it needs drawing.
    let pulse = show_logo && color && keys.active();

    writeln!(output).map_err(io)?;
    let mut card = intro_card(width, color, if pulse { "2;32" } else { "1;32" }, show_logo);
    for line in &card {
        writeln!(output, "{line}").map_err(io)?;
    }
    if pulse {
        output.flush().map_err(io)?;
        std::thread::sleep(LOGO_PULSE);
        rewind_block(output, card.len())?;
        card = intro_card(width, color, "1;32", show_logo);
        for line in &card {
            writeln!(output, "{line}").map_err(io)?;
        }
    }

    let where_ = match scope {
        Scope::Global => "this machine ($HOME)",
        Scope::Repository => "this repository",
    };
    let driving = detected.map_or(String::new(), |agent| {
        format!("  ·  driving: {}", agent.name())
    });
    writeln!(
        output,
        "{}",
        paint(color, "2", &format!("scope: {where_}{driving}"))
    )
    .map_err(io)?;
    for line in help_lines(
        &format!("writes under {}", base.display()),
        row_width(),
        color,
    ) {
        writeln!(output, "{line}").map_err(io)?;
    }
    writeln!(output, "{}", paint(color, "2", &list_keys_hint(keys))).map_err(io)?;
    Ok(())
}

/// The key hints the list questions share, for the input mode this run has.
fn list_keys_hint(keys: &KeyReader) -> String {
    if keys.active() {
        "↑/↓ or ←/→ move · space select · a all · n none · enter continue · esc cancel".to_string()
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
                index: agent.index(),
                label: format!("{}{note}", agent.name()),
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
                index: feature.index(),
                label: feature.label().to_string(),
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

/// One selectable row: its 1-based index (shown only in typed mode), the label
/// without that index, the help shown while the cursor is on it, and whether
/// this scope can use it at all.
struct Row {
    index: usize,
    label: String,
    help: &'static str,
    disabled: bool,
}

/// A row's cursor and checkbox. The checkbox is bright green when checked; the
/// cursor row is bold. Typed mode shows the number in the label, arrow mode
/// drops it.
fn row_prefix(cursor: bool, checked: bool, color: bool) -> String {
    let pointer = if cursor {
        paint(color, "1", "❯")
    } else {
        " ".to_string()
    };
    let checkbox = match (cursor, checked) {
        (true, true) => paint(color, "1;92", "◉"),
        (false, true) => paint(color, "92", "◉"),
        _ => "◯".to_string(),
    };
    format!("{pointer} {checkbox} ")
}

/// A row's label, with the index prepended in typed mode and the note a row
/// this scope cannot use carries. The long reason is the help line under the
/// list.
fn row_label(row: &Row, show_numbers: bool) -> String {
    let text = if show_numbers {
        format!("{}. {}", row.index, row.label)
    } else {
        row.label.clone()
    };
    if row.disabled {
        format!("{text} (not in this scope)")
    } else {
        text
    }
}

/// A row as drawn: the checkbox on the first line, the label wrapped under it,
/// then styled. A row this scope cannot use is dim; the row under the cursor is
/// bold, its checkbox bright green.
///
/// The label is wrapped *before* the prefix is added: `wrap` collapses runs of
/// whitespace, so wrapping the prefixed line would eat the columns the cursor
/// and the checkbox occupy and pull every row to the left edge.
fn row_lines(
    row: &Row,
    cursor: bool,
    checked: bool,
    width: usize,
    color: bool,
    show_numbers: bool,
) -> Vec<String> {
    let prefix = row_prefix(cursor, checked, color);
    let label = row_label(row, show_numbers);
    let prefix_width = crate::select::visible_len(&prefix);
    let room = width.saturating_sub(prefix_width);
    let wrapped = wrap(&label, room.max(1), prefix_width);

    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let text = if cursor && !row.disabled {
                paint(color, "1", &text)
            } else {
                text
            };
            let line = if i == 0 {
                format!("{prefix}{text}")
            } else {
                text
            };
            if row.disabled {
                paint(color, "2", &line)
            } else {
                line
            }
        })
        .collect()
}

/// The lines the list draws: every row, a blank line, then the help of the row
/// under the cursor.
///
/// The blank line is not decoration: with the help directly under the rows it
/// reads as one more option, which is how a reader takes it at a glance.
fn list_block(
    rows: &[Row],
    cursor: usize,
    checked: &[bool],
    width: usize,
    color: bool,
) -> Vec<String> {
    let mut block: Vec<String> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        block.extend(row_lines(
            row,
            index == cursor,
            checked.get(index).copied().unwrap_or(false),
            width,
            color,
            false,
        ));
    }
    block.push(String::new());
    block.extend(help_lines(
        rows.get(cursor).map_or("", |row| row.help),
        width,
        color,
    ));
    block
}

/// The focused row's help, dim, wrapped, with a dim `▏` gutter.
///
/// `wrap` is asked for no indent of its own: the two spaces and gutter are added
/// here to every line, so a continuation would otherwise carry extra columns
/// and drift right of the sentence it continues.
fn help_lines(text: &str, width: usize, color: bool) -> Vec<String> {
    let gutter = paint(color, "2", "▏");
    let prefix_width = 2 + crate::select::visible_len(&gutter) + 1;
    wrap(text, width.saturating_sub(prefix_width), 0)
        .into_iter()
        .map(|line| format!("  {gutter} {}", paint(color, "2", &line)))
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
        // Erase the block drawn last time and paint it again: every line is
        // one terminal row, because `wrap` fitted it to the width, so the
        // count is exact.
        if drawn > 0 {
            rewind_block(output, drawn)?;
        }
        let block = list_block(rows, state.cursor, &state.checked, row_width(), color);
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
                // the next key does not paint one line too low. The erase
                // leaves the screen blank until then, which is what the next
                // key's paint fills.
                rewind_block(output, drawn)?;
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
            for line in row_lines(row, false, checked[index], width, color, true) {
                writeln!(output, "{line}").map_err(io)?;
            }
        }
        // The typed path has no cursor, so the focused row's help is never
        // shown: a row this scope cannot use has to carry its reason here, or
        // the reader never learns how to enable it. The blank line keeps both
        // the reasons and the answer line clear of the list above them.
        writeln!(output).map_err(io)?;
        for row in rows.iter().filter(|row| row.disabled) {
            for line in help_lines(row.help, width, color) {
                writeln!(output, "{line}").map_err(io)?;
            }
        }
        if rows.iter().any(|row| row.disabled) {
            writeln!(output).map_err(io)?;
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
    let what = match plan.scope {
        Scope::Global => "this machine",
        Scope::Repository => "this project",
    };
    writeln!(output, "Here's what pixel setup will set up for {what}.").map_err(io)?;
    writeln!(
        output,
        "{}",
        paint(
            color,
            "2",
            &format!("Writes under {}", display_base(&plan.base, plan.scope))
        )
    )
    .map_err(io)?;

    if !hooks.is_empty() {
        section(output, color, "Hooks and agent configuration")?;
        writeln!(
            output,
            "{}",
            paint(
                color,
                "2",
                "What the installer will do, from its own dry run."
            )
        )
        .map_err(io)?;
        for step in hooks {
            item(output, color, &format!("{}: {}", step.id, step.summary))?;
            if let Some(detail) = &step.detail {
                let gutter = paint(color, "2", "▏");
                writeln!(output, "  {gutter} {}", paint(color, "2", detail)).map_err(io)?;
            }
        }
    }
    if !plan.config_writes.is_empty() {
        section(output, color, "Global settings")?;
        for key in &plan.config_writes {
            item(
                output,
                color,
                &format!(
                    "{}: {}",
                    key.key.join("."),
                    render_value(key.value.to_json())
                ),
            )?;
        }
    }
    if !plan.notes.is_empty() {
        section(output, color, "Heads up")?;
        for note in &plan.notes {
            writeln!(
                output,
                "{} {}",
                paint(color, "1;33", "\u{26a0}"),
                wrap(note, row_width().saturating_sub(2), 0).join("\n  ")
            )
            .map_err(io)?;
        }
    }

    section(output, color, "The skill's text")?;
    let gutter = paint(color, "2", "▏");
    for line in pixel_setup::skill::PIXEL_SKILL.lines() {
        writeln!(output, "  {gutter} {}", paint(color, "2", line)).map_err(io)?;
    }
    Ok(())
}

/// A review section: a bold bright-green heading with an arrow marker, set off
/// from what comes before it by a blank line.
fn section(output: &mut impl Write, color: bool, title: &str) -> Result<(), String> {
    writeln!(output).map_err(io)?;
    writeln!(output, "{}", paint(color, "1;92", &format!("▸ {title}"))).map_err(io)
}

/// One line of what the run will do: a bright-green check, then the thing itself.
fn item(output: &mut impl Write, color: bool, text: &str) -> Result<(), String> {
    writeln!(output, "{} {text}", paint(color, "92", "\u{2714}")).map_err(io)
}

/// The home directory, for collapsing a path to `~`.
fn home_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The run's base as the review shows it: `~` when it is the home directory,
/// its own path otherwise (a repository, or the development dummy root).
fn display_base(base: &Path, scope: Scope) -> String {
    match scope {
        Scope::Global if home_path().as_deref() == Some(base) => "~".to_string(),
        _ => base.display().to_string(),
    }
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

/// The prefix for an Apply/Cancel row: a bold cursor when focused, a filled
/// diamond for Apply and a hollow one for Cancel.
fn confirm_prefix(cursor: bool, yes: bool, color: bool) -> String {
    let pointer = if cursor {
        paint(color, "1", "❯")
    } else {
        " ".to_string()
    };
    let diamond = if yes {
        paint(color, "92", "◆")
    } else {
        "◇".to_string()
    };
    format!("{pointer} {diamond} ")
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
        // Erase the block drawn last time, then paint it again: one terminal
        // row per line, because `wrap` fitted it to the width.
        if drawn > 0 {
            rewind_block(output, drawn)?;
        }
        let width = row_width();
        let mut block: Vec<String> = Vec::new();
        for (index, (label, _)) in CHOICES.iter().enumerate() {
            let yes = index == 0;
            let prefix = confirm_prefix(index == picked, yes, color);
            let text = if index == picked {
                paint(color, "1", label)
            } else {
                label.to_string()
            };
            block.push(format!("{prefix}{text}"));
        }
        // The blank line the list questions use, for the same reason.
        block.push(String::new());
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
            // repainted. The rewind erases anyway, because the next key can be
            // an arrow, and the help of the other choice can be shorter than
            // the one on screen: a bare rewind would leave its tail behind.
            ListKey::Space | ListKey::All => {
                rewind_block(output, drawn)?;
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
    hooks: &InstallReport,
    scripted: bool,
    screens: &Screens<'_>,
) -> Result<(), String> {
    let color = screens.color;
    writeln!(output).map_err(io)?;
    if plan.config_writes.is_empty() && hooks.steps.is_empty() {
        writeln!(
            output,
            "{}",
            paint(color, "2", "pixel setup wrote nothing.")
        )
        .map_err(io)?;
        for note in &plan.notes {
            writeln!(output, "  note: {note}").map_err(io)?;
        }
    } else {
        let width = banner_width();
        // A card row that runs past the width would push the right border
        // out: every line is wrapped to fit before it is boxed.
        let inner = width.saturating_sub(4);
        let box_lines = |output: &mut dyn Write, text: &str| -> Result<(), String> {
            for line in wrap(text, inner, 0) {
                writeln!(output, "{}", box_row(color, &line, width)).map_err(io)?;
            }
            Ok(())
        };
        writeln!(output, "╭{}╮", "─".repeat(width)).map_err(io)?;
        writeln!(
            output,
            "{}",
            box_row(
                color,
                &format!(
                    "{} {}",
                    paint(color, "92", "✓"),
                    paint(color, "1", "pixel setup complete")
                ),
                width,
            )
        )
        .map_err(io)?;
        for step in &hooks.steps {
            box_lines(output, &format!("  {}: {}", step.id, step.summary))?;
        }
        for key in &plan.config_writes {
            box_lines(
                output,
                &format!(
                    "  set {} = {}",
                    key.key.join("."),
                    render_value(key.value.to_json())
                ),
            )?;
        }
        if !plan.notes.is_empty() {
            writeln!(output, "{}", box_row(color, "", width)).map_err(io)?;
            for note in &plan.notes {
                box_lines(output, &format!("  note: {note}"))?;
            }
        }
        if !scripted {
            writeln!(output, "{}", box_row(color, "", width)).map_err(io)?;
            let next = format!(
                "{} {}",
                paint(color, "92", "next ›"),
                paint(color, "92", "pixel doctor .")
            );
            writeln!(output, "{}", box_row(color, &next, width)).map_err(io)?;
        }
        writeln!(output, "╰{}╯", "─".repeat(width)).map_err(io)?;
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
// REMOVAL (before the release, tracked in docs/archive/design/pixel-setup-plan.md and
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

    use pixel_setup::setup::{AgentTarget, Feature, Scope};

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
             docs/archive/design/pixel-setup-plan.md says how to test with them.",
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

    /// Rows for a test list.
    fn rows(labels: &[&str]) -> Vec<Row> {
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| Row {
                index: index + 1,
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
    fn a_blank_line_sets_the_focused_help_off_from_the_rows() {
        let list = rows(&["a", "b"]);
        let block = list_block(&list, 1, &[true, false], 40, false);
        let help = block
            .iter()
            .position(|line| line.contains("the help line"))
            .expect("the focused row's help");
        assert_eq!(
            block[help - 1],
            "",
            "the help reads as another row without it: {block:?}"
        );
        assert_eq!(
            block[help - 2],
            "❯ ◯ b",
            "the row above the gap is the cursor's own row: {block:?}"
        );

        // The typed path draws the same gap before its answer line.
        let (_, written) = with_input("\n", |input, output| {
            ask(input, output, &rows(&["a", "b"]), &[1])
        });
        let lines: Vec<&str> = written.lines().collect();
        let answer = lines
            .iter()
            .position(|line| line.contains("Your answer"))
            .expect("the answer line");
        assert_eq!(lines[answer - 1], "", "got {written}");
    }

    #[test]
    fn a_step_banner_is_always_preceded_by_a_blank_line() {
        let keys = KeyReader::inert();
        let mut screens = Screens {
            steps: Steps::new(2),
            color: false,
            keys: &keys,
        };
        let mut out = Vec::new();
        // The block a list draws, help and all, then the next step's banner.
        for line in list_block(&rows(&["a"]), 0, &[true], 20, false) {
            writeln!(out, "{line}").unwrap();
        }
        screens.steps.banner(&mut out, "Review", false).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let title = lines
            .iter()
            .position(|line| line.contains("Review"))
            .expect("the banner title");
        assert_eq!(
            lines[title - 1],
            "",
            "a step starts on its own line, however long the help above it was: {text:?}"
        );
        assert!(
            lines[title + 1].chars().all(|c| c == '─'),
            "the rule follows the title: {text:?}"
        );
    }

    #[test]
    fn a_wrapped_help_continuation_is_indented_once_not_twice() {
        let help = "one two three four five six seven eight nine ten eleven twelve";
        let lines = help_lines(help, 20, false);
        assert!(lines.len() > 1, "the help wraps: {lines:?}");
        for line in &lines {
            assert_eq!(
                line.chars().take_while(|c| *c == ' ').count(),
                2,
                "every line of the help is indented two columns, once: {lines:?}"
            );
            assert!(
                crate::select::visible_len(line) <= 20,
                "and fits the width it was given: {lines:?}"
            );
        }
    }

    #[test]
    fn the_manual_notes_are_indented_once_too() {
        let scratch = scratch_dir("notes-indent");
        let mut plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode],
            &[Feature::Prompt],
        )
        .unwrap();
        // A note that fills the two-column reservation twice over, so the wrap
        // and the indent are both at the boundary they have to hold.
        let note_width = row_width().saturating_sub(2);
        plan.notes = vec!["x".repeat(note_width * 2 + 1)];
        let keys = KeyReader::inert();
        let (rendered, written) = with_input("", |_, output| {
            write_review(output, &plan, &[], &mut screens(&keys))
        });
        rendered.unwrap();
        let after_heading = written
            .split("Heads up")
            .nth(1)
            .expect("the heads-up section");
        let note_lines: Vec<&str> = after_heading
            .lines()
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        assert!(note_lines.len() > 1, "the note wraps: {after_heading:?}");
        assert!(
            note_lines[0].starts_with('\u{26a0}'),
            "the first line carries the warning mark: {after_heading:?}"
        );
        for line in &note_lines {
            assert!(
                crate::select::visible_len(line) <= row_width(),
                "the warning mark and the indent are inside the reservation, \
                 so no line reaches the terminal's last column: {after_heading:?}"
            );
        }
        for line in &note_lines[1..] {
            assert_eq!(
                line.chars().take_while(|c| *c == ' ').count(),
                2,
                "a wrapped note lines up two columns in, not four: {after_heading:?}"
            );
        }
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn the_step_banner_right_aligns_the_counter_and_never_touches_the_title() {
        // "Agents" is 6 columns and "Step 1 of 3" is 11, so a 40-column banner
        // leaves 23 spaces between them.
        assert_eq!(banner_gap("Agents", "Step 1 of 3", 40), 23);
        assert_eq!(
            banner_gap("Agents", "Step 1 of 3", 17),
            1,
            "a title that exactly fills the banner still gets one space"
        );
        assert_eq!(
            banner_gap("A title past the banner", "Step 1 of 3", 18),
            1,
            "a title longer than the banner keeps one space, it is not cut"
        );
    }

    #[test]
    fn both_key_hints_are_the_ones_each_input_mode_shows() {
        assert_eq!(
            list_keys_hint(&KeyReader::forced(true)),
            "↑/↓ or ←/→ move · space select · a all · n none · enter continue · esc cancel"
        );
        assert_eq!(
            list_keys_hint(&KeyReader::forced(false)),
            "Type numbers (1,3), `all` or `none` · enter keeps the shown defaults · q cancels"
        );
    }

    #[test]
    fn no_repaint_rewinds_by_hand_instead_of_through_rewind_block() {
        // A bare rewind only overwrites, so the tail of a longer line stays on
        // screen — twice in this file's history, once in each loop. The erase
        // lives in `rewind_block`; this is what keeps every rewind going
        // through it. The needle is assembled so the assertion cannot match
        // itself.
        let source = include_str!("setup_cmd.rs");
        let bare = concat!("\\x1b[", "{drawn}A\"");
        assert!(
            !source.contains(bare),
            "a hand-written rewind reappeared: call `rewind_block` instead"
        );
        assert!(
            source.contains("fn rewind_block"),
            "and the primitive it should call still exists"
        );
    }

    #[test]
    fn erasing_a_block_moves_back_to_its_first_line_and_clears_it() {
        let mut written = Vec::new();
        rewind_block(&mut written, 7).unwrap();
        assert_eq!(
            String::from_utf8(written).unwrap(),
            "\x1b[7A\r\x1b[J",
            "seven lines up, to the line start, then erase to the end of the screen"
        );
    }

    #[test]
    fn a_repaint_leaves_no_tail_of_the_block_before_it() {
        // The old help was three lines, the new one is one: the erase is what
        // stops the last two from staying on screen.
        let keys = KeyReader::inert();
        let mut written = Vec::new();
        let mut steps = Steps::new(1);
        rewind_block(&mut written, 4).unwrap();
        let erased = String::from_utf8(written).unwrap();
        assert!(
            erased.contains("\x1b[J"),
            "a repaint that only overwrites leaves the old tail: {erased:?}"
        );
        let _ = (&keys, &mut steps);
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
        assert!(written.contains("  ◉ 1. a"), "got {written}");
        assert!(written.contains("  ◯ 2. b"), "got {written}");
        assert!(written.contains("  ◉ 3. c"), "got {written}");
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
            written.contains("◯ 2. land (not in this scope)"),
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
            index: 12,
            label: "Land on the target branch, and the rest of a long label".into(),
            help: "help",
            disabled: false,
        };
        let lines = row_lines(&row, true, true, 24, false, false);
        assert!(lines.len() > 1, "a label past the width wraps: {lines:?}");
        assert!(lines[0].starts_with("❯ ◉ "), "{lines:?}");
        assert_eq!(
            lines[1].chars().take_while(|c| *c == ' ').count(),
            row_prefix(true, true, false).chars().count(),
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
            index: 3,
            label: "Cursor".into(),
            help: "help",
            disabled: false,
        };
        let on = row_lines(&row, true, true, 40, true, false);
        assert!(on[0].contains("\x1b[1;92m"), "the checkbox: {on:?}");
        assert!(
            on[0].ends_with("\x1b[0m") || on[0].contains("\x1b[1m"),
            "the label carries the cursor: {on:?}"
        );
        let plain = row_lines(&row, true, true, 40, false, false);
        assert_eq!(plain, vec!["❯ ◉ Cursor"], "no SGR when colour is off");
    }

    #[test]
    fn a_disabled_row_is_dim_and_keeps_its_reason_out_of_the_label() {
        let row = Row {
            index: 12,
            label: "Land".into(),
            help: "why",
            disabled: true,
        };
        let lines = row_lines(&row, false, false, 40, false, true);
        assert_eq!(lines, vec!["  ◯ 12. Land (not in this scope)"]);
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
    fn the_review_prints_every_step_key_and_note_before_the_text() {
        let scratch = scratch_dir("review");
        let plan = SetupPlan::new(
            &scratch,
            Scope::Global,
            &[AgentTarget::ClaudeCode, AgentTarget::AgentMd],
            &[Feature::Prompt, Feature::Metrics],
        )
        .unwrap();
        let hooks = [
            step("hooks.claude", "would register the task-event hooks"),
            step("skills.pixel", "would write skills/pixel/SKILL.md"),
        ];
        let keys = KeyReader::inert();
        let (rendered, written) = with_input("", |_, output| {
            write_review(output, &plan, &hooks, &mut screens(&keys))
        });
        rendered.unwrap();

        assert!(written.contains("hooks.claude"), "{written}");
        assert!(written.contains("skills.pixel"), "{written}");
        assert!(written.contains("metrics: \"on\""), "{written}");
        assert!(written.contains("brief: true"), "{written}");
        assert!(
            written.contains("no harness directory"),
            "the agent with nothing to write is named: {written}"
        );
        assert!(
            written.contains("The skill's text"),
            "the deployed skill text is shown: {written}"
        );
        assert!(
            written.contains("name: pixel"),
            "the skill front matter is shown: {written}"
        );
        let text = written.find("The skill's text").unwrap();
        let step = written.find("skills.pixel").unwrap();
        assert!(
            step < text,
            "the install steps come before the text they deploy"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    fn step(id: &str, summary: &str) -> InstallStep {
        InstallStep {
            id: id.into(),
            status: pixel_setup::install::CheckStatus::Green,
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
            summary: pixel_setup::install::InstallSummary {
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
            &[Feature::Prompt],
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
            written.find("hooks.claude").unwrap() < written.find("The skill's text").unwrap(),
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
            &[Feature::Prompt],
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
            &[Feature::Classify],
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
        )
        .unwrap();
        let hooks = install_report(vec![step("hooks.claude", "registered the hooks")]);
        let keys = KeyReader::inert();
        let (_, written) = with_input("", |_, output| {
            report(output, &plan, &hooks, false, &screens(&keys))
        });
        assert!(written.contains("pixel setup complete"), "{written}");
        assert!(
            written.contains("hooks.claude: registered the hooks"),
            "{written}"
        );
        assert!(written.contains("pixel doctor"), "{written}");

        let empty_plan =
            SetupPlan::new(&scratch, Scope::Global, &[AgentTarget::ClaudeCode], &[]).unwrap();
        let (_, empty) = with_input("", |_, output| {
            report(
                output,
                &empty_plan,
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
        assert_eq!(default_features(Scope::Global).len(), 3);

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
            let answers = dev::answers(&options, pixel_setup::setup::Scope::Global)
                .unwrap()
                .expect("both flags answered");
            assert_eq!(
                answers.agents,
                vec![
                    pixel_setup::setup::AgentTarget::ClaudeCode,
                    pixel_setup::setup::AgentTarget::Devin
                ]
            );
            assert!(!dev::scripted(&Options {
                selected_agents: Some("1".into()),
                ..Options::default()
            }));
        }

        #[test]
        fn a_cancel_cannot_be_typed_into_a_flag() {
            let options = Options {
                selected_agents: Some("q".into()),
                selected_features: Some("q".into()),
                dummy_apply: false,
            };
            assert!(dev::answers(&options, pixel_setup::setup::Scope::Global).is_err());
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
                refusal.contains("docs/archive/design/pixel-setup-plan.md"),
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
