// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel ultraflow` — the classify-driven browser loop over saved flows.
//!
//! Two verbs, and the loop between them:
//!
//! - `discover` drives the page toward a goal with `pixel classify` deciding
//!   one operation-target pair per cycle, then composes what worked into a
//!   `pixel flow` document and saves it under the flow store, where
//!   `pixel flow` can list, show and revise it like any other flow.
//! - `replay` follows a saved flow. Its `conditional` steps are decided by
//!   `pixel classify` (the text matcher is the disclosed fallback), and a
//!   step whose page no longer matches is re-decided once — `--update`
//!   records that new branch into the flow, so the next replay is
//!   deterministic where this one had to think.
//!
//! Both verbs drive `agent-browser` on PATH (`--session comet`, the session
//! `pixel flow run` uses); a missing or unbound browser is that
//! command's error, reported as-is.

use std::collections::HashMap;

use pixel_flow::{Flow, FlowStep};
use pixel_ultraflow::{
    Composed, Decider, Decision, DiscoverRequest, Distribution, FlowMeta, Limits, ReplayRequest,
    Status, Trace, TracedStep, Var,
};
use serde_json::json;

/// The engine inputs a caller chooses, shared by both verbs: the same four
/// `pixel classify` takes, so an ultraflow run resolves its engine exactly
/// as a classify invocation does.
#[derive(clap::Args, Debug, Clone)]
pub struct DeciderFlags {
    /// Decision engine: `remote` (an OpenAI-compatible chat completion) or
    /// `ollaya` (the local decision daemon). Without it the stored
    /// preference decides, exactly as `pixel classify` resolves one.
    #[arg(long, value_enum)]
    pub engine: Option<crate::classify::EngineChoice>,
    /// Remote provider preset (overrides the stored choice; defaults to the
    /// stored one, then `openrouter`). Selects the base URL and the API-key
    /// environment variable — see `pixel classify --help`.
    #[arg(long, value_enum)]
    pub remote_preset: Option<crate::decide_remote::Preset>,
    /// Remote model id (overrides the preset default).
    #[arg(long)]
    pub remote_model: Option<String>,
    /// Base URL of the local Ollaya server (with the local engine).
    #[arg(long, default_value_t = crate::decide_ollaya::DEFAULT_BASE.to_string())]
    pub ollaya_url: String,
}

#[derive(clap::Args, Debug)]
pub struct UltraflowOptions {
    #[command(subcommand)]
    pub cmd: UltraflowCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum UltraflowCmd {
    /// Drive a page toward a goal with `pixel classify`, one operation per
    /// cycle, and compose what worked into a flow document.
    ///
    /// `--save <name>` writes it into the flow store, where
    /// `pixel flow list|show` finds it. `--repeat 2` runs the
    /// goal twice and composes a `conditional` where the two runs took
    /// different paths, so a replay chooses by condition instead of
    /// following one frozen route.
    Discover {
        /// The page to start from.
        #[arg(long)]
        url: String,
        /// What to accomplish, in one sentence. Becomes the flow's goal:
        /// every page decision and every value question is framed by it.
        #[arg(long)]
        goal: String,
        /// A value a field may take: `--var key=value`. Repeatable. A typed
        /// field is filled from a declared variable or from a string the
        /// goal itself contains — never from an invented value.
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
        /// Save the composed flow under this name. Without it the run only
        /// reports what it did.
        #[arg(long)]
        save: Option<String>,
        /// Flow title (defaults to the goal).
        #[arg(long)]
        title: Option<String>,
        /// Tag for the saved flow; repeatable (`--tag auth --tag web`).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Most actions one run may take.
        #[arg(long, default_value_t = pixel_ultraflow::discover::DEFAULT_MAX_STEPS)]
        max_steps: usize,
        /// Consecutive actions that leave the page unchanged before the run
        /// stops as blocked.
        #[arg(long, default_value_t = pixel_ultraflow::discover::DEFAULT_MAX_STALLED)]
        max_stalled: usize,
        /// Run the goal this many times and compose the traces together.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        #[command(flatten)]
        decider: DeciderFlags,
        #[arg(long)]
        json: bool,
    },
    /// Follow a saved flow, deciding its `conditional` steps with `pixel classify`.
    Replay {
        /// Flow name, as `pixel flow list` prints it.
        name: String,
        /// A value for the flow's `value_var`: `--var key=value`. Repeatable.
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
        /// Do not re-decide a step whose page no longer matches.
        #[arg(long)]
        no_repair: bool,
        /// Most steps one replay may re-decide.
        #[arg(long, default_value_t = pixel_ultraflow::replay::DEFAULT_MAX_REPAIRS)]
        max_repairs: usize,
        /// Record each re-decided step into the flow as a `conditional`
        /// branch, so the next replay chooses instead of thinking again.
        #[arg(long)]
        update: bool,
        #[command(flatten)]
        decider: DeciderFlags,
        #[arg(long)]
        json: bool,
    },
}

/// `pixel classify` behind the ultraflow decision seam.
///
/// One engine is opened for the whole run: `pixel ultraflow` asks one
/// question per cycle, so it pays the engine resolution and the connection
/// once instead of once per decision.
struct ClassifyDecider {
    engine: Box<dyn crate::classify::DecisionEngine>,
}

impl ClassifyDecider {
    // Opens the real classify engine over the network: the resolution policy
    // is `open_session`'s (tested in `classify`), and the budget rule it
    // forwards is `option_budget_for` below.
    #[cfg_attr(test, mutants::skip)]
    fn open(cfg: &DeciderFlags) -> Result<ClassifyDecider, String> {
        Ok(ClassifyDecider {
            engine: crate::classify::open_session(
                cfg.engine,
                cfg.ollaya_url.clone(),
                cfg.remote_preset,
                cfg.remote_model.clone(),
            )?,
        })
    }
}

/// How many options one question may offer the engine behind `provider`.
///
/// The schema ceiling is 255, but a decision head has its own budget: the
/// local Ollaya model refuses a 65th option (`422`), so a caller that
/// builds a question has to offer fewer. A remote chat completion has no
/// such head and takes the ceiling.
fn option_budget_for(provider: Option<&str>) -> usize {
    match provider {
        Some("ollaya") => crate::decide_ollaya::MAX_OPTIONS,
        _ => crate::decide_ollaya::MAX_LABELS,
    }
}

impl Decider for ClassifyDecider {
    // Forwards the engine's own provider to the tested budget rule.
    #[cfg_attr(test, mutants::skip)]
    fn option_budget(&self) -> usize {
        option_budget_for(self.engine.provider())
    }

    // The adapter over the real engine: `Spec::checked` validates, the
    // engine answers. Both are tested where they live.
    #[cfg_attr(test, mutants::skip)]
    fn decide(&mut self, request: &Decision) -> Result<Distribution, String> {
        // The same validation, the same caps, the same engine as a
        // `pixel classify` invocation: a label-less or one-label question
        // is refused here rather than asked badly.
        let spec = crate::classify::Spec::checked(
            request.text.clone(),
            request.context.clone(),
            request.labels.clone(),
            request.criteria.clone(),
        )?;
        let probabilities = self.engine.decide(&spec)?;
        Ok(Distribution {
            probabilities,
            model: self.engine.model_id(),
        })
    }
}

pub fn run(opts: UltraflowOptions) -> Result<(), String> {
    match opts.cmd {
        UltraflowCmd::Discover {
            url,
            goal,
            vars,
            save,
            title,
            tags,
            max_steps,
            max_stalled,
            repeat,
            decider,
            json,
        } => discover(&DiscoverCommand {
            url,
            goal,
            vars: parse_vars(&vars)?,
            save,
            title,
            tags,
            limits: Limits::clamped(max_steps, max_stalled),
            repeat,
            decider,
            json,
        }),
        UltraflowCmd::Replay {
            name,
            vars,
            no_repair,
            max_repairs,
            update,
            decider,
            json,
        } => replay(&ReplayCommand {
            name,
            vars: parse_vars(&vars)?,
            repair: repairs_are_allowed(no_repair),
            max_repairs,
            update,
            decider,
            json,
        }),
    }
}

/// `--no-repair` is a negation on the command line while the walk takes the
/// positive, so the polarity lives in one named place: a replay repairs a
/// step it can repair unless the caller turned that off.
fn repairs_are_allowed(no_repair: bool) -> bool {
    !no_repair
}

/// `--var key=value`, in the order the caller gave them.
fn parse_vars(pairs: &[String]) -> Result<Vec<Var>, String> {
    pairs
        .iter()
        .map(|pair| {
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("--var expects key=value, got '{pair}'"))?;
            if name.is_empty() {
                return Err(format!("--var expects a name before '=', got '{pair}'"));
            }
            Ok(Var::new(name, value))
        })
        .collect()
}

struct DiscoverCommand {
    url: String,
    goal: String,
    vars: Vec<Var>,
    save: Option<String>,
    title: Option<String>,
    tags: Vec<String>,
    limits: Limits,
    repeat: usize,
    decider: DeciderFlags,
    json: bool,
}

// Drives the real `agent-browser` and the real engine for a whole run: the
// loop, its bounds and its composition are `pixel_ultraflow`'s, tested there
// against scripted seams. What is left here is wiring and printing.
#[cfg_attr(test, mutants::skip)]
fn discover(cfg: &DiscoverCommand) -> Result<(), String> {
    let mut decider = ClassifyDecider::open(&cfg.decider)?;
    let mut browser = pixel_flow::agent_browser();
    let mut traces: Vec<Trace> = Vec::new();
    let mut log = format!(
        "# ultraflow discover — {}\n# page: {}\n\n",
        cfg.goal, cfg.url
    );

    for attempt in 1..=cfg.repeat.max(1) {
        let request = DiscoverRequest {
            goal: &cfg.goal,
            url: &cfg.url,
            vars: &cfg.vars,
            limits: cfg.limits,
        };
        let trace = pixel_ultraflow::discover(&mut browser, &mut decider, &request)?;
        log.push_str(&render_trace(attempt, &trace));
        let stuck = trace.status == Status::Blocked;
        traces.push(trace);
        if stuck {
            // Another attempt would meet the same page.
            log.push_str("# a run blocked: not repeating it\n");
            break;
        }
    }

    let composed = match &cfg.save {
        Some(name) => Some(save_flow(name, cfg, &traces, &mut log)?),
        None => None,
    };
    if cfg.json {
        let traces: Vec<&Trace> = traces.iter().collect();
        let body = json!({
            "traces": traces,
            "flow": composed.as_ref().map(|composed| &composed.flow),
            "warnings": composed.as_ref().map_or_else(Vec::new, |composed| {
                composed.warnings.iter().map(|warning| json!(warning)).collect()
            }),
            "saved": composed.as_ref().map(|composed| composed.path.clone()),
            "log": log,
        });
        return crate::print_data(&body, true);
    }
    print!("{log}");
    if let Some(composed) = &composed {
        println!(
            "# composed flow: {} ({} steps) -> {}",
            composed.flow.name,
            composed.flow.steps.len(),
            composed.path
        );
    }
    Ok(())
}

/// A composed flow and where it was written.
#[derive(Debug)]
struct Saved {
    flow: Flow,
    path: String,
    warnings: Vec<String>,
}

/// Compose the traces and write the flow into the flow store.
///
/// A name that is already taken is refused rather than overwritten: the
/// stored flow may be one a hand-written revision produced, and the agent
/// that owns it is not this run to replace.
fn save_flow(
    name: &str,
    cfg: &DiscoverCommand,
    traces: &[Trace],
    log: &mut String,
) -> Result<Saved, String> {
    let slug = pixel_flow::slugify(name);
    if pixel_flow::exists(&slug) {
        return Err(format!(
            "flow '{slug}' already exists — pass a different --save name, or revise it with \
             `pixel flow revise {slug} --from-file <path>`"
        ));
    }
    let meta = FlowMeta {
        name: slug.clone(),
        title: cfg.title.clone().unwrap_or_else(|| cfg.goal.clone()),
        tags: cfg.tags.clone(),
    };
    let Composed { mut flow, warnings } = pixel_ultraflow::compose(traces, &meta)?;
    let now = pixel_flow::now_unix();
    flow.created_unix = now;
    flow.revised_unix = now;
    let path = pixel_flow::save(&flow)?;
    let path = path.display().to_string();
    for warning in &warnings {
        log.push_str(&format!("# warning: {warning}\n"));
    }
    Ok(Saved {
        flow,
        path,
        warnings,
    })
}

/// One run's report, one line per cycle.
fn render_trace(attempt: usize, trace: &Trace) -> String {
    let mut out = format!(
        "# run {attempt}: {} decisions, {} steps\n",
        trace.decisions,
        trace.steps.len()
    );
    for (index, step) in trace.steps.iter().enumerate() {
        out.push_str(&render_step(index + 1, step));
    }
    for refused in &trace.refused {
        out.push_str(&format!("# refused: {refused}\n"));
    }
    out.push_str(&format!(
        "# {}: {}\n\n",
        trace.status.as_str(),
        trace.detail
    ));
    out
}

/// One recorded cycle: what was decided, the step it produced, and whether
/// the page moved under it.
fn render_step(index: usize, step: &TracedStep) -> String {
    format!(
        "{index:>3}. {} (p={:.2}) -> {} [{}]\n",
        step.decision.label,
        step.decision.probability,
        describe_step(&step.step),
        if step.changed {
            "page moved"
        } else {
            "no change"
        }
    )
}

/// What a recorded step does, for a human reading the run.
fn describe_step(step: &FlowStep) -> String {
    let target = step.ref_hint.clone().unwrap_or_else(|| "-".to_string());
    let mut out = format!("{} {target}", step.action);
    if let Some(value) = step.value.as_deref().filter(|value| !value.is_empty()) {
        out.push_str(&format!(" = \"{value}\""));
    }
    if let Some(name) = &step.value_var {
        out.push_str(&format!(" ({name})"));
    }
    if let Some(wait) = &step.wait {
        out.push_str(&format!(" {wait}"));
    }
    out
}

struct ReplayCommand {
    name: String,
    vars: Vec<Var>,
    repair: bool,
    max_repairs: usize,
    update: bool,
    decider: DeciderFlags,
    json: bool,
}

// Drives the real `agent-browser` and the real engine: the walk, the
// conditions and the repairs are `pixel_ultraflow`'s, tested there.
#[cfg_attr(test, mutants::skip)]
fn replay(cfg: &ReplayCommand) -> Result<(), String> {
    let mut flow = pixel_flow::load(&cfg.name)?;
    let vars: HashMap<String, String> = cfg
        .vars
        .iter()
        .map(|var| (var.name.clone(), var.value.clone()))
        .collect();
    let mut decider = ClassifyDecider::open(&cfg.decider)?;
    let mut browser = pixel_flow::agent_browser();
    let report = pixel_ultraflow::replay(
        &mut browser,
        &mut decider,
        &ReplayRequest {
            flow: &flow,
            vars: &vars,
            repair: cfg.repair,
            max_repairs: cfg.max_repairs,
        },
    );
    let updated = match cfg.update {
        true => apply_deviations(&mut flow, &report)?,
        false => 0,
    };
    if cfg.json {
        let body = json!({
            "flow": flow.name,
            "revision": flow.revision,
            "report": report,
            "updated": updated,
        });
        crate::print_data(&body, true)?;
    } else {
        print!("{}", report.log);
        println!(
            "# {} steps executed, {} skipped, {} conditions decided, {} deviations",
            report.steps_executed,
            report.steps_skipped,
            report.conditions.len(),
            report.deviations.len()
        );
        if updated > 0 {
            println!(
                "# recorded {updated} re-decided step(s) into '{}' as revision {}",
                flow.name, flow.revision
            );
        }
    }
    match outcome_error(&report) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The error a replay report is: its own, or the outcome it did not reach.
///
/// A run whose steps all ran but whose outcome checks failed is not a
/// success, and the command must say which of the two happened.
fn outcome_error(report: &pixel_ultraflow::ReplayReport) -> Option<String> {
    match &report.error {
        Some(error) => Some(error.clone()),
        None if !report.success => Some("the flow did not reach its outcome".to_string()),
        None => None,
    }
}

/// Record every re-decided step into the flow as a `conditional` branch, and
/// bump the revision. Returns how many were recorded.
///
/// Nothing is recorded for a replay that failed: the branches it found were
/// taken on a page the flow never reached, and persisting them would brand
/// the flow with a path nobody proved. A deviation the flow cannot name (a
/// step with no element to ask the page about) is left alone for the same
/// reason — a wrong condition is worse than a re-decision.
fn apply_deviations(
    flow: &mut Flow,
    report: &pixel_ultraflow::ReplayReport,
) -> Result<usize, String> {
    if outcome_error(report).is_some() {
        return Ok(0);
    }
    let mut recorded = 0;
    for deviation in &report.deviations {
        let Some(repaired) = deviation.repaired.clone() else {
            continue;
        };
        let Some(step) = pixel_ultraflow::compose::locate_mut(&mut flow.steps, &deviation.step)
        else {
            continue;
        };
        let Some(wrapped) = pixel_ultraflow::compose::wrap_with_fallback(step, repaired) else {
            continue;
        };
        *step = wrapped;
        recorded += 1;
    }
    if recorded > 0 {
        flow.revision += 1;
        flow.revised_unix = pixel_flow::now_unix();
        pixel_flow::save(flow)?;
    }
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_is_read_from_the_key_value_form() {
        let vars = parse_vars(&["from=Zurich".to_string(), "to=London".to_string()]).unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].name, "from");
        assert_eq!(vars[0].value, "Zurich");
        assert_eq!(vars[0].description, "from");
        assert_eq!(vars[1].value, "London");
        // An empty value is a value: clearing a field is a real request.
        let empty = parse_vars(&["q=".to_string()]).unwrap();
        assert_eq!(empty[0].value, "");
    }

    /// The local model's budget is measured, not the schema ceiling: a page
    /// that overflows it must be told so here, because the engine refuses
    /// an over-budget question rather than truncating it.
    #[test]
    fn the_option_budget_follows_the_engine_a_run_resolved_to() {
        assert_eq!(
            option_budget_for(Some("ollaya")),
            crate::decide_ollaya::MAX_OPTIONS
        );
        assert_eq!(crate::decide_ollaya::MAX_OPTIONS, 64);
        // A chat completion, and an engine whose provider is unknown (a
        // test engine), take the schema ceiling.
        for provider in [Some("openrouter"), Some("ollama"), None] {
            assert_eq!(
                option_budget_for(provider),
                crate::decide_ollaya::MAX_LABELS,
                "{provider:?}"
            );
        }
    }

    /// The flag is a negation, the walk takes the positive: inverting it
    /// would make `--no-repair` the flag that enables repair.
    #[test]
    fn repair_is_on_unless_the_caller_turned_it_off() {
        assert!(repairs_are_allowed(false));
        assert!(!repairs_are_allowed(true));
    }

    #[test]
    fn a_variable_without_a_name_is_refused() {
        assert_eq!(
            parse_vars(&["Zurich".to_string()]).unwrap_err(),
            "--var expects key=value, got 'Zurich'"
        );
        assert_eq!(
            parse_vars(&["=Zurich".to_string()]).unwrap_err(),
            "--var expects a name before '=', got '=Zurich'"
        );
    }

    /// The human log is the run's visible output: the cycles, the refused
    /// ones, and how it ended.
    #[test]
    fn a_run_renders_one_line_per_cycle_and_its_ending() {
        let trace = Trace {
            url: "https://e.com/".to_string(),
            goal: "Search for flights".to_string(),
            status: Status::Budget,
            detail: "stopped after 2 steps without reaching DONE".to_string(),
            decisions: 3,
            refused: vec!["the step for CLICK 2 did not run: unknown ref".to_string()],
            steps: vec![
                traced_step(1, "CLICK 2", 0.91, true),
                traced_step(2, "TYPE 3", 0.5, false),
            ],
        };
        // Built line by line: a `\` continuation would swallow the two
        // spaces the index is padded with.
        let expected = [
            "# run 2: 3 decisions, 2 steps\n",
            "  1. CLICK 2 (p=0.91) -> click button containing 'Search' [page moved]\n",
            "  2. TYPE 3 (p=0.50) -> fill textbox containing 'Where from?' = \"Zurich\" (from) [no change]\n",
            "# refused: the step for CLICK 2 did not run: unknown ref\n",
            "# budget: stopped after 2 steps without reaching DONE\n\n",
        ]
        .concat();
        assert_eq!(render_trace(2, &trace), expected);
    }

    fn traced_step(index: usize, label: &str, probability: f64, changed: bool) -> TracedStep {
        let (action, hint, value, value_var) = if index == 1 {
            ("click", "button containing 'Search'", None, None)
        } else {
            (
                "fill",
                "textbox containing 'Where from?'",
                Some("Zurich"),
                Some("from"),
            )
        };
        TracedStep {
            decision: pixel_ultraflow::DecisionRecord {
                label: label.to_string(),
                probability,
                model: "winnow:e4b".to_string(),
                offered: 9,
                truncated: 0,
            },
            step: FlowStep {
                action: action.to_string(),
                ref_hint: Some(hint.to_string()),
                value: value.map(ToString::to_string),
                value_var: value_var.map(ToString::to_string),
                ..Default::default()
            },
            value: value.map(ToString::to_string),
            value_label: None,
            value_source: None,
            snapshot_before: String::new(),
            snapshot_after: String::new(),
            url_before: String::new(),
            url_after: String::new(),
            changed,
            fill_skipped: false,
            log: String::new(),
        }
    }

    /// A run whose outcome checks failed is not a success, and the command
    /// must report which of the two happened.
    #[test]
    fn a_replays_error_is_its_own_or_the_outcome_it_missed() {
        let failed = pixel_ultraflow::ReplayReport {
            error: Some(
                "no element matching 'button containing 'Gone'' found in snapshot".to_string(),
            ),
            ..Default::default()
        };
        assert_eq!(
            outcome_error(&failed).as_deref(),
            Some("no element matching 'button containing 'Gone'' found in snapshot")
        );
        // The steps ran, the outcome did not hold.
        let missed = pixel_ultraflow::ReplayReport {
            success: false,
            ..Default::default()
        };
        assert_eq!(
            outcome_error(&missed).as_deref(),
            Some("the flow did not reach its outcome")
        );
        let reached = pixel_ultraflow::ReplayReport {
            success: true,
            ..Default::default()
        };
        assert_eq!(outcome_error(&reached), None);
    }

    /// A scratch flow store for one unit test: the process has one
    /// `PIXEL_FLOW_DIR`, so the name carries the test's own name.
    fn scratch_flow_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-ultraflow-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `--update` records a re-decided step into the flow, bumps the
    /// revision, and leaves a deviation it cannot place alone.
    #[test]
    fn updating_records_each_placed_deviation_and_skips_the_rest() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let dir = scratch_flow_dir("update");
        // SAFETY: ENV_LOCK serialises every test that touches process-wide
        // variables, and PIXEL_FLOW_DIR is one of them.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &dir);
        }
        let mut flow = Flow {
            name: "sign-in".to_string(),
            title: "Sign in".to_string(),
            description: "Sign in".to_string(),
            tags: vec!["ultraflow".to_string()],
            url: Some("https://e.com/".to_string()),
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps: vec![
                FlowStep {
                    action: "open".to_string(),
                    url: Some("https://e.com/".to_string()),
                    ..Default::default()
                },
                FlowStep {
                    action: "click".to_string(),
                    ref_hint: Some("button containing 'Sign in'".to_string()),
                    ..Default::default()
                },
            ],
            success_signal: None,
            created_unix: 1,
            revised_unix: 1,
            revision: 1,
            proven: false,
        };
        // A report whose replay reached its outcome: only then do the
        // deviations get recorded.
        let report = pixel_ultraflow::ReplayReport {
            success: true,
            deviations: vec![
                deviation("2", Some(click_step("button containing 'Continue'"))),
                // A step the flow cannot name is left alone.
                deviation(
                    "2",
                    Some(FlowStep {
                        action: "wait".to_string(),
                        wait: Some("500ms".to_string()),
                        ..Default::default()
                    }),
                ),
                // A step the flow no longer has is left alone.
                deviation("9", Some(click_step("button containing 'Elsewhere'"))),
                // A deviation with no repaired step is left alone.
                deviation("2", None),
            ],
            ..Default::default()
        };
        // This report carries no error, so the deviations are recorded.
        assert_eq!(apply_deviations(&mut flow, &report).unwrap(), 1);
        assert_eq!(flow.revision, 2);

        // A replay that failed records nothing: its branches were taken on
        // a page the flow never reached.
        let mut failed = flow.clone();
        failed.revision = 5;
        let failed_report = pixel_ultraflow::ReplayReport {
            deviations: report.deviations.clone(),
            error: Some("the flow ran, but the outcome checks did not pass".to_string()),
            ..Default::default()
        };
        assert_eq!(apply_deviations(&mut failed, &failed_report).unwrap(), 0);
        assert_eq!(failed.revision, 5, "not bumped");
        // Zero recorded placements do not bump the revision either: the flow
        // on disk is exactly the one the last replay read.
        let mut empty = flow.clone();
        empty.revision = 9;
        let no_placement = pixel_ultraflow::ReplayReport {
            success: true,
            deviations: vec![deviation("2", None), deviation("9", Some(click_step("x")))],
            ..Default::default()
        };
        assert_eq!(apply_deviations(&mut empty, &no_placement).unwrap(), 0);
        assert_eq!(empty.revision, 9, "no placements, no bump");
        let wrapped = &flow.steps[1];
        assert_eq!(wrapped.action, "conditional");
        assert_eq!(wrapped.condition.as_deref(), Some("page shows 'Sign in'"));
        assert_eq!(
            wrapped.then[0].ref_hint.as_deref(),
            Some("button containing 'Sign in'")
        );
        assert_eq!(
            wrapped.otherwise[0].ref_hint.as_deref(),
            Some("button containing 'Continue'")
        );
        // The revision is on disk, so the next replay reads the branch.
        let stored = pixel_flow::load("sign-in").unwrap();
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.steps[1].action, "conditional");

        // Nothing placed: no revision bump, no write.
        let mut untouched = flow.clone();
        untouched.revision = 7;
        let nothing = pixel_ultraflow::ReplayReport::default();
        assert_eq!(apply_deviations(&mut untouched, &nothing).unwrap(), 0);
        assert_eq!(untouched.revision, 7);
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }

    fn click_step(hint: &str) -> FlowStep {
        FlowStep {
            action: "click".to_string(),
            ref_hint: Some(hint.to_string()),
            ..Default::default()
        }
    }

    fn deviation(step: &str, repaired: Option<FlowStep>) -> pixel_ultraflow::Deviation {
        pixel_ultraflow::Deviation {
            step: step.to_string(),
            failure: "no element found".to_string(),
            decided: Some("CLICK 1".to_string()),
            detail: String::new(),
            repaired,
        }
    }

    /// A `--save` name that is already taken is refused rather than
    /// overwritten: the flow on disk may be one a person revised.
    #[test]
    fn saving_over_an_existing_flow_is_refused() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let dir = scratch_flow_dir("save");
        // SAFETY: ENV_LOCK serialises every test that touches process-wide
        // variables, and PIXEL_FLOW_DIR is one of them.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &dir);
        }
        let cfg = DiscoverCommand {
            url: "https://e.com/".to_string(),
            goal: "Sign in".to_string(),
            vars: Vec::new(),
            save: Some("sign-in".to_string()),
            title: None,
            tags: Vec::new(),
            limits: Limits::default(),
            repeat: 1,
            decider: DeciderFlags {
                engine: None,
                remote_preset: None,
                remote_model: None,
                ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            },
            json: false,
        };
        let mut log = String::new();
        // Nothing is named yet, so the refusal is the only way this returns
        // without a trace to compose.
        assert!(save_flow("sign-in", &cfg, &[], &mut log).is_err());
        let stored = Flow {
            name: "sign-in".to_string(),
            title: "Sign in".to_string(),
            description: String::new(),
            tags: vec![],
            url: None,
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps: vec![click_step("button containing 'Sign in'")],
            success_signal: None,
            created_unix: 1,
            revised_unix: 1,
            revision: 3,
            proven: false,
        };
        pixel_flow::save(&stored).unwrap();
        let err = save_flow("sign in", &cfg, &[], &mut log).unwrap_err();
        assert_eq!(
            err,
            "flow 'sign-in' already exists — pass a different --save name, or revise it with \
             `pixel flow revise sign-in --from-file <path>`"
        );
        assert_eq!(
            pixel_flow::load("sign-in").unwrap().revision,
            3,
            "not overwritten"
        );
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }

    #[test]
    fn a_recorded_step_reads_as_the_command_it_produces() {
        assert_eq!(
            describe_step(&FlowStep {
                action: "click".to_string(),
                ref_hint: Some("button containing 'Search'".to_string()),
                ..Default::default()
            }),
            "click button containing 'Search'"
        );
        assert_eq!(
            describe_step(&FlowStep {
                action: "fill".to_string(),
                ref_hint: Some("textbox containing 'Where from?'".to_string()),
                value: Some("Zurich".to_string()),
                value_var: Some("from".to_string()),
                ..Default::default()
            }),
            "fill textbox containing 'Where from?' = \"Zurich\" (from)"
        );
        assert_eq!(
            describe_step(&FlowStep {
                action: "wait".to_string(),
                wait: Some("500ms".to_string()),
                ..Default::default()
            }),
            "wait - 500ms"
        );
        assert_eq!(
            describe_step(&FlowStep {
                action: "scroll".to_string(),
                value: Some("down 800".to_string()),
                ..Default::default()
            }),
            "scroll - = \"down 800\""
        );
        // An empty value is not a value: rendering it would read as a step
        // that types nothing.
        assert_eq!(
            describe_step(&FlowStep {
                action: "clear".to_string(),
                ref_hint: Some("textbox containing 'Where from?'".to_string()),
                value: Some(String::new()),
                ..Default::default()
            }),
            "clear textbox containing 'Where from?'"
        );
    }
}
