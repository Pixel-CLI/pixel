//! Composition: traces in, a saved flow out.
//!
//! A single run composes to one plain step per action. Two runs of the same
//! goal that took different paths compose to a `conditional`: the branch a
//! replay should take is written as a condition over the page, so the flow
//! carries the *different conditions* of how to do the task instead of one
//! frozen path that only works on the day it was recorded.

use std::collections::BTreeSet;

use pixel_flow::{Flow, FlowStep, FlowVar};

use crate::discover::{Status, Trace};
use crate::elements::parse_snapshot;

/// The instruction a discovered step carries for a replay that finds the
/// page no longer matching it. `pixel ultraflow replay` acts on it; a plain
/// `pixel flow run` reads it as documentation.
pub const DECIDE_ON_FAILURE: &str =
    "re-decide one cycle with `pixel classify` on the current page and record what worked";

/// The instruction a replay finds on a step the composer calls decidable.
pub fn is_decidable(step: &FlowStep) -> bool {
    step.on_failure.as_deref() == Some(DECIDE_ON_FAILURE)
}

/// The flow identity a caller saves the composition under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowMeta {
    pub name: String,
    pub title: String,
    pub tags: Vec<String>,
}

/// A composed flow, and what could not be expressed in it.
#[derive(Debug, Clone)]
pub struct Composed {
    pub flow: Flow,
    pub warnings: Vec<String>,
}

/// Turn one or more runs of the same goal into a flow.
///
/// Several runs are how a branch is discovered: where they diverge, the
/// flow records a condition instead of picking one of the paths.
pub fn compose(traces: &[Trace], meta: &FlowMeta) -> Result<Composed, String> {
    let first = traces.first().ok_or("no trace to compose")?;
    if let Some(mismatched) = traces.iter().find(|trace| trace.url != first.url) {
        return Err(format!(
            "the traces start at different urls, so one flow cannot describe them: {} and {}",
            first.url, mismatched.url
        ));
    }
    if traces.iter().all(|trace| trace.steps.is_empty()) {
        return Err(format!("no run recorded a step: {}", first.detail));
    }
    let mut warnings = Vec::new();
    let usable: Vec<&Trace> = traces
        .iter()
        .filter(|trace| !trace.steps.is_empty())
        .collect();
    if usable.len() < traces.len() {
        warnings.push(format!(
            "{} of {} runs recorded no step and were not composed",
            traces.len() - usable.len(),
            traces.len()
        ));
    }
    if let Some(early) = usable.iter().find(|trace| trace.status != Status::Done) {
        warnings.push(format!(
            "a composed run ended {} rather than done: {}",
            early.status.as_str(),
            early.detail
        ));
    }

    let (mut steps, merge_warnings) = merge(&usable);
    warnings.extend(merge_warnings);
    steps.insert(0, crate::discover::start_step(&first.url));
    for step in &mut steps {
        mark_decidable(step);
    }

    let success_signal = success_signal(&usable);
    if success_signal.is_none() {
        warnings.push(
            "no listed element appeared on the final page that was absent from the first, so the \
             flow carries no success signal; verify the outcome independently"
                .to_string(),
        );
    }
    let flow = Flow {
        name: meta.name.clone(),
        title: meta.title.clone(),
        description: first.goal.clone(),
        tags: if meta.tags.is_empty() {
            vec!["ultraflow".to_string()]
        } else {
            meta.tags.clone()
        },
        url: Some(first.url.clone()),
        tab: None,
        success_url_contains: Vec::new(),
        success_url_excludes: Vec::new(),
        mfa_keywords: Vec::new(),
        stale_tab_cleanup: Vec::new(),
        preconditions: vec![
            "verify the final outcome independently: a completed run is not proof of success"
                .to_string(),
        ],
        vars: declared_vars(&usable),
        steps,
        success_signal,
        created_unix: 0,
        revised_unix: 0,
        revision: 1,
        proven: false,
    };
    flow.validate()?;
    Ok(Composed { flow, warnings })
}

/// The step a replay id names, for revision: `2` is the second root step,
/// `2t.1` the first step of its `then` branch, `2o.1` of its `otherwise`.
pub fn locate_mut<'a>(steps: &'a mut Vec<FlowStep>, path: &str) -> Option<&'a mut FlowStep> {
    let mut segments = path.split('.').peekable();
    let mut current = steps;
    while let Some(segment) = segments.next() {
        let (digits, branch) = match segment.strip_suffix('t') {
            Some(digits) => (digits, Some("t")),
            None => match segment.strip_suffix('o') {
                Some(digits) => (digits, Some("o")),
                None => (segment, None),
            },
        };
        let index = digits.parse::<usize>().ok()?.checked_sub(1)?;
        let step = current.get_mut(index)?;
        if segments.peek().is_none() {
            // A branch letter on the last segment names a branch, not a
            // step: there is nothing to revise.
            return branch.is_none().then_some(step);
        }
        current = match branch {
            Some("t") => &mut step.then,
            Some("o") => &mut step.otherwise,
            _ => return None,
        };
    }
    None
}

/// Wrap a step and the step a re-decision found in its place, so the next
/// replay of the flow chooses between them instead of re-deciding: it runs
/// the recorded step when the page still shows what that step targeted, and
/// the discovered one otherwise.
///
/// `None` for a step whose target the condition cannot name — a step with
/// no element (`open`, `wait`) is not a step a page moved away from.
pub fn wrap_with_fallback(step: &FlowStep, repaired: FlowStep) -> Option<FlowStep> {
    let targeted = first_quoted(step.ref_hint.as_deref()?)?;
    let mut wrapped = FlowStep {
        action: "conditional".to_string(),
        condition: Some(format!("page shows '{targeted}'")),
        rationale: Some(format!(
            "a replay did not find this step's page and re-decided it with pixel classify; the \
             recorded step is kept for the page it was discovered on (it targeted '{targeted}')"
        )),
        then: vec![step.clone()],
        otherwise: vec![repaired],
        ..Default::default()
    };
    mark_decidable(&mut wrapped);
    Some(wrapped)
}

/// The first single-quoted term of a `ref_hint`.
fn first_quoted(hint: &str) -> Option<String> {
    let open = hint.find('\'')?;
    let rest = &hint[open + 1..];
    // A hint's term runs to its LAST quote, so a name that itself carries an
    // apostrophe (`'Don't have an account?'`) is not cut in half — a halved
    // condition would match unrelated pages.
    let close = rest.rfind('\'')?;
    Some(rest[..close].to_string())
}

/// Mark every step a replay may re-decide, at every nesting depth.
fn mark_decidable(step: &mut FlowStep) {
    if step.on_failure.is_none() {
        step.on_failure = Some(DECIDE_ON_FAILURE.to_string());
    }
    if step.max_retries == 0 {
        step.max_retries = 1;
    }
    for branch in step.then.iter_mut().chain(step.otherwise.iter_mut()) {
        mark_decidable(branch);
    }
}

/// The variables a set of traces used, declared with the recorded value as
/// their default: a replay with no `--var` still runs, and one with a
/// `--var` overrides it.
fn declared_vars(traces: &[&Trace]) -> Vec<FlowVar> {
    let mut declared: Vec<FlowVar> = Vec::new();
    for trace in traces {
        for traced in &trace.steps {
            let (Some(name), Some(value)) = (traced.step.value_var.as_ref(), traced.value.as_ref())
            else {
                continue;
            };
            if declared.iter().any(|var| &var.name == name) {
                continue;
            }
            declared.push(FlowVar {
                name: name.clone(),
                description: format!(
                    "value entered in \"{}\"",
                    traced.step.ref_hint.as_deref().unwrap_or(name)
                ),
                required: false,
                default: Some(value.clone()),
            });
        }
    }
    declared
}

/// The condition a replay can check for success: an element every composed
/// run's final page lists and its first page did not.
///
/// Every run, not just the first: a composed flow can take any of its
/// branches, and the outcome check runs after whichever one the replay took.
/// A signal, not proof — the flow's preconditions say so.
fn success_signal(traces: &[&Trace]) -> Option<String> {
    let (first, rest) = traces.split_first()?;
    let gained = |trace: &Trace| -> Option<BTreeSet<String>> {
        let before = names(&trace.steps.first()?.snapshot_before);
        Some(
            parse_snapshot(&trace.steps.last()?.snapshot_after)
                .0
                .into_iter()
                .map(|element| element.name)
                .filter(|name| !name.is_empty() && !before.contains(name))
                .collect(),
        )
    };
    // Snapshot order of the first run, filtered to names every other run
    // gained too.
    gained(first)?
        .into_iter()
        .find(|name| {
            rest.iter()
                .all(|trace| gained(trace).is_some_and(|gained| gained.contains(name)))
        })
        .map(|name| format!("page shows '{name}'"))
}

/// The names an observation lists.
fn names(snapshot: &str) -> BTreeSet<String> {
    parse_snapshot(snapshot)
        .0
        .into_iter()
        .map(|element| element.name)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The steps a set of traces agrees on, with a conditional where they
/// diverged.
fn merge(traces: &[&Trace]) -> (Vec<FlowStep>, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(first) = traces.first() else {
        return (Vec::new(), warnings);
    };
    let longest = traces
        .iter()
        .map(|trace| trace.steps.len())
        .max()
        .unwrap_or(0);
    let divergence = (0..longest).find(|index| {
        let key = prefix_key(first, *index);
        traces.iter().any(|trace| prefix_key(trace, *index) != key)
    });
    let Some(divergence) = divergence else {
        // Every run took the same path: no condition to record.
        return (
            first
                .steps
                .iter()
                .map(|traced| traced.step.clone())
                .collect(),
            warnings,
        );
    };

    let mut steps: Vec<FlowStep> = first.steps[..divergence]
        .iter()
        .map(|traced| traced.step.clone())
        .collect();
    let mut branches: Vec<Branch<'_>> = Vec::new();
    for trace in traces {
        let key = suffix_key(trace, divergence);
        if let Some(known) = branches.iter_mut().find(|branch| branch.key == key) {
            known.runs.push(trace);
            continue;
        }
        branches.push(Branch {
            key,
            runs: vec![trace],
        });
    }
    // A run that ended exactly at the divergence took no alternative path:
    // recording it as a branch would splice two different paths into one
    // sequence.
    let offered = branches.len();
    branches.retain(|branch| !branch.steps(divergence).is_empty());
    if branches.len() < offered {
        warnings.push(format!(
            "{} run(s) ended at step {} without a next action, so they are not a branch of it",
            offered - branches.len(),
            divergence + 1
        ));
    }
    if branches.len() == 1 {
        steps.extend(branches[0].steps(divergence));
        return (steps, warnings);
    }

    // The last distinct branch is the fallback: a condition has to name
    // something *present* in the branch that takes it, so one branch is
    // left as the `otherwise`.
    let fallback = branches.len() - 1;
    let mut chain = branches[fallback].steps(divergence);
    for (index, branch) in branches.iter().enumerate().rev() {
        if index == fallback {
            continue;
        }
        let Some(condition) = distinguishing_condition(branch, &branches, divergence) else {
            warnings.push(format!(
                "a run diverged onto a path no condition can name (its page lists nothing the \
                 other paths do not), so the flow follows the fallback there instead: {}",
                branch.label(divergence)
            ));
            continue;
        };
        chain = vec![FlowStep {
            action: "conditional".to_string(),
            condition: Some(condition),
            rationale: Some(format!(
                "two discovered runs diverged here; this branch ran {}",
                branch.label(divergence)
            )),
            then: branch.steps(divergence),
            otherwise: chain,
            ..Default::default()
        }];
    }
    steps.extend(chain);
    (steps, warnings)
}

/// One distinct continuation of the traces past the divergence.
struct Branch<'a> {
    key: String,
    runs: Vec<&'a Trace>,
}

impl Branch<'_> {
    /// The branch's steps, from `divergence` on. Empty for a run that
    /// ended at the divergence.
    fn steps(&self, divergence: usize) -> Vec<FlowStep> {
        self.runs[0]
            .steps
            .get(divergence..)
            .unwrap_or_default()
            .iter()
            .map(|traced| traced.step.clone())
            .collect()
    }

    /// The branch in one line, for a warning or a rationale. `merge` retains
    /// only branches with steps, so this always names at least one decision.
    fn label(&self, divergence: usize) -> String {
        self.runs[0]
            .steps
            .get(divergence..)
            .unwrap_or_default()
            .iter()
            .map(|traced| traced.decision.label.as_str())
            .collect::<Vec<_>>()
            .join(" then ")
    }

    /// The names this branch's page lists at the divergence. All runs in a
    /// branch share the suffix, so its first run speaks for the branch.
    fn page_names(&self, divergence: usize) -> BTreeSet<String> {
        self.runs[0]
            .steps
            .get(divergence)
            .map_or_else(BTreeSet::new, |traced| names(&traced.snapshot_before))
    }
}

/// A condition true on this branch's page and false on every other
/// branch's: an element only this branch's page lists.
///
/// Phrased in the vocabulary `pixel_flow::evaluate_condition` already
/// understands, so the classify evaluator and the text fallback read the
/// same sentence.
fn distinguishing_condition(
    branch: &Branch<'_>,
    branches: &[Branch<'_>],
    divergence: usize,
) -> Option<String> {
    let theirs: BTreeSet<String> = branches
        .iter()
        .filter(|other| other.key != branch.key)
        .flat_map(|other| other.page_names(divergence))
        .collect();
    branch.runs[0]
        .steps
        .get(divergence)
        .and_then(|traced| {
            parse_snapshot(&traced.snapshot_before)
                .0
                .into_iter()
                .map(|element| element.name)
                .find(|name| !name.is_empty() && !theirs.contains(name))
        })
        .map(|name| format!("page shows '{name}'"))
}

/// A key for the first `index + 1` steps of a run. A run that stopped
/// earlier yields a shorter key, so where one run went on and another did
/// not is a divergence like any other.
fn prefix_key(trace: &Trace, index: usize) -> String {
    let taken = (index + 1).min(trace.steps.len());
    steps_key(&trace.steps[..taken])
}

/// A key for a run's continuation from `from`: the same steps produce the
/// same key, and a run with nothing left yields the empty key.
fn suffix_key(trace: &Trace, from: usize) -> String {
    steps_key(
        trace
            .steps
            .get(from.min(trace.steps.len())..)
            .unwrap_or_default(),
    )
}

/// The key of a run of steps: two runs produce it exactly when a replay
/// would do the same thing.
fn steps_key(steps: &[crate::discover::TracedStep]) -> String {
    steps
        .iter()
        .map(|traced| {
            format!(
                "{}|{}|{}|{};",
                traced.step.action,
                traced.step.ref_hint.as_deref().unwrap_or(""),
                traced.step.value.as_deref().unwrap_or(""),
                traced.step.value_var.as_deref().unwrap_or("")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::{DecisionRecord, TracedStep};

    const PAGE_A: &str = "- heading \"Sign in\" [ref=e1]\n- button \"Continue\" [ref=e2]";
    const PAGE_B: &str =
        "- heading \"Welcome back\" [ref=e1]\n- button \"Select account\" [ref=e9]";
    const PAGE_C: &str = "- heading \"Two-factor\" [ref=e1]\n- textbox \"Code\" [ref=e5]";

    const URL: &str = "https://e.com/login";

    fn traced(
        action: &str,
        hint: Option<&str>,
        value: Option<&str>,
        value_var: Option<&str>,
        before: &str,
        after: &str,
    ) -> TracedStep {
        TracedStep {
            decision: DecisionRecord {
                label: action.to_uppercase(),
                probability: 0.9,
                model: "winnow:e4b".to_string(),
                offered: 9,
                truncated: 0,
            },
            step: FlowStep {
                action: action.to_string(),
                ref_hint: hint.map(ToString::to_string),
                value: value.map(ToString::to_string),
                value_var: value_var.map(ToString::to_string),
                ..Default::default()
            },
            value: value.map(ToString::to_string),
            value_label: value_var.map(|name| format!("VAR {name}")),
            value_source: None,
            snapshot_before: before.to_string(),
            snapshot_after: after.to_string(),
            url_before: URL.to_string(),
            url_after: URL.to_string(),
            changed: true,
            fill_skipped: false,
            log: String::new(),
        }
    }

    fn meta() -> FlowMeta {
        FlowMeta {
            name: "sign-in".to_string(),
            title: "Sign in".to_string(),
            tags: vec![],
        }
    }

    fn run(steps: Vec<TracedStep>) -> Trace {
        Trace {
            url: URL.to_string(),
            goal: "Sign in and open the dashboard".to_string(),
            status: Status::Done,
            detail: "the decision reported the goal complete after 1 steps".to_string(),
            decisions: steps.len(),
            steps,
            refused: Vec::new(),
        }
    }

    fn click(hint: &str, before: &str, after: &str) -> TracedStep {
        traced("click", Some(hint), None, None, before, after)
    }

    #[test]
    fn one_run_composes_to_plain_steps_in_order() {
        let composed = compose(
            &[run(vec![
                click("button containing 'Continue'", PAGE_A, PAGE_B),
                traced(
                    "fill",
                    Some("textbox containing 'Code'"),
                    Some("ABCD"),
                    Some("code"),
                    PAGE_B,
                    PAGE_B,
                ),
            ])],
            &meta(),
        )
        .unwrap();
        let flow = &composed.flow;
        assert_eq!(flow.name, "sign-in");
        assert_eq!(flow.title, "Sign in");
        assert_eq!(flow.description, "Sign in and open the dashboard");
        assert_eq!(flow.tags, ["ultraflow"]);
        assert_eq!(flow.url.as_deref(), Some(URL));
        assert_eq!(flow.steps.len(), 3);
        assert_eq!(flow.steps[0].action, "open");
        assert_eq!(flow.steps[0].url.as_deref(), Some(URL));
        assert_eq!(
            flow.steps[0].rationale.as_deref(),
            Some("start of the discovered path")
        );
        assert_eq!(flow.steps[1].action, "click");
        assert_eq!(flow.steps[2].action, "fill");
        assert_eq!(flow.steps[2].value_var.as_deref(), Some("code"));
        // The recorded value is the variable's default: a replay with no
        // `--var` still runs.
        assert_eq!(flow.vars.len(), 1);
        assert_eq!(flow.vars[0].name, "code");
        assert_eq!(flow.vars[0].default.as_deref(), Some("ABCD"));
        assert!(!flow.vars[0].required);
        assert!(
            flow.vars[0]
                .description
                .contains("textbox containing 'Code'")
        );
        // Every step a replay might fail on carries the re-decide
        // instruction and a retry.
        assert!(flow.steps.iter().all(is_decidable));
        assert!(flow.steps.iter().all(|step| step.max_retries == 1));
        // The success signal is an element the final page gained that the
        // first page did not list.
        assert_eq!(
            flow.success_signal.as_deref(),
            Some("page shows 'Select account'")
        );
        assert!(!flow.proven);
        assert!(composed.warnings.is_empty(), "{:?}", composed.warnings);
        flow.validate().unwrap();
    }

    /// The headline: two runs that took different paths compose to a
    /// conditional whose condition names what tells the paths apart.
    #[test]
    fn two_diverging_runs_compose_to_a_conditional() {
        let composed = compose(
            &[
                run(vec![click("link containing 'Sign in'", PAGE_A, PAGE_B)]),
                run(vec![click(
                    "button containing 'Select account'",
                    PAGE_B,
                    PAGE_A,
                )]),
            ],
            &meta(),
        )
        .unwrap();
        let steps = &composed.flow.steps;
        assert_eq!(steps.len(), 2, "{steps:#?}");
        assert_eq!(steps[0].action, "open");
        assert_eq!(steps[1].action, "conditional");
        assert_eq!(steps[1].condition.as_deref(), Some("page shows 'Sign in'"));
        assert_eq!(
            steps[1].then[0].ref_hint.as_deref(),
            Some("link containing 'Sign in'")
        );
        assert_eq!(
            steps[1].otherwise[0].ref_hint.as_deref(),
            Some("button containing 'Select account'")
        );
        assert!(
            steps[1]
                .rationale
                .as_deref()
                .unwrap()
                .contains("this branch ran CLICK"),
            "{:?}",
            steps[1].rationale
        );
        // The branches end on different pages, so no element is guaranteed
        // after either: the composer says the signal is missing instead of
        // picking one a replay could fail on.
        assert!(
            composed
                .warnings
                .iter()
                .any(|warning| warning.contains("carries no success signal")),
            "{:?}",
            composed.warnings
        );
        // The nested branches are marked decidable and retried too.
        assert!(steps[1].then.iter().all(is_decidable));
        assert!(steps[1].otherwise.iter().all(|step| step.max_retries == 1));
        composed.flow.validate().unwrap();
    }

    #[test]
    fn agreeing_runs_compose_to_a_plain_path() {
        let path = || {
            run(vec![
                click("button containing 'Continue'", PAGE_A, PAGE_B),
                traced("press", None, None, None, PAGE_B, PAGE_B),
            ])
        };
        let composed = compose(&[path(), path()], &meta()).unwrap();
        assert_eq!(composed.flow.steps.len(), 3, "{:#?}", composed.flow.steps);
        assert!(
            composed
                .flow
                .steps
                .iter()
                .all(|step| step.action != "conditional")
        );
        assert!(composed.warnings.is_empty(), "{:?}", composed.warnings);
    }

    #[test]
    fn three_paths_compose_to_a_chain_of_conditionals() {
        let composed = compose(
            &[
                run(vec![click("a-hint", PAGE_A, PAGE_B)]),
                run(vec![click("b-hint", PAGE_B, PAGE_C)]),
                run(vec![click("c-hint", PAGE_C, PAGE_A)]),
            ],
            &meta(),
        )
        .unwrap();
        let steps = &composed.flow.steps;
        assert_eq!(steps.len(), 2);
        let outer = &steps[1];
        assert_eq!(outer.action, "conditional");
        assert_eq!(outer.condition.as_deref(), Some("page shows 'Sign in'"));
        assert_eq!(outer.then[0].ref_hint.as_deref(), Some("a-hint"));
        let inner = &outer.otherwise;
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].action, "conditional");
        assert_eq!(
            inner[0].condition.as_deref(),
            Some("page shows 'Welcome back'")
        );
        assert_eq!(inner[0].then[0].ref_hint.as_deref(), Some("b-hint"));
        assert_eq!(inner[0].otherwise[0].ref_hint.as_deref(), Some("c-hint"));
        // The branches end on different pages, so no element is guaranteed
        // after either: the composer says the signal is missing instead of
        // picking one a replay could fail on.
        assert!(
            composed
                .warnings
                .iter()
                .any(|warning| warning.contains("carries no success signal")),
            "{:?}",
            composed.warnings
        );
        composed.flow.validate().unwrap();
    }

    /// A branch whose page lists nothing the others do not cannot be named
    /// by a condition, so the flow follows the fallback there and says so —
    /// it never records a condition that is false on its own branch.
    #[test]
    fn a_branch_no_condition_can_name_is_disclosed_not_invented() {
        let composed = compose(
            &[
                run(vec![click("a-hint", PAGE_A, PAGE_B)]),
                run(vec![click("b-hint", PAGE_A, PAGE_B)]),
            ],
            &meta(),
        )
        .unwrap();
        let steps = &composed.flow.steps;
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[1].action, "click",
            "the unnameable branch is not recorded as a condition"
        );
        assert_eq!(steps[1].ref_hint.as_deref(), Some("b-hint"));
        assert_eq!(composed.warnings.len(), 1, "{:?}", composed.warnings);
        assert!(
            composed.warnings[0].contains("no condition can name"),
            "{:?}",
            composed.warnings
        );
        // The warning names the path it gave up on, decision by decision.
        assert!(
            composed.warnings[0].ends_with("instead: CLICK"),
            "{:?}",
            composed.warnings
        );
    }

    /// A run that ended at the divergence is not an alternative path: the
    /// flow records the path that went on, and says the other was dropped.
    #[test]
    fn a_run_that_ended_at_the_divergence_is_disclosed() {
        let short = run(vec![click("a-hint", PAGE_A, PAGE_B)]);
        let long = run(vec![
            click("a-hint", PAGE_A, PAGE_B),
            click("b-hint", PAGE_B, PAGE_B),
        ]);
        let composed = compose(&[short, long], &meta()).unwrap();
        assert_eq!(composed.flow.steps.len(), 3, "{:#?}", composed.flow.steps);
        let actions: Vec<&str> = composed
            .flow
            .steps
            .iter()
            .map(|step| step.action.as_str())
            .collect();
        assert_eq!(
            actions,
            ["open", "click", "click"],
            "the shared step is recorded once, then the path that went on"
        );
        assert_eq!(composed.flow.steps[2].ref_hint.as_deref(), Some("b-hint"));
        assert_eq!(composed.warnings.len(), 1, "{:?}", composed.warnings);
        assert!(
            composed.warnings[0].contains("ended at step 2"),
            "{:?}",
            composed.warnings
        );
    }

    #[test]
    fn a_run_that_recorded_nothing_is_refused_or_disclosed() {
        let mut empty = run(vec![]);
        empty.status = Status::Budget;
        empty.detail = "stopped after 40 steps without reaching DONE".to_string();
        assert_eq!(
            compose(&[empty.clone()], &meta()).unwrap_err(),
            "no run recorded a step: stopped after 40 steps without reaching DONE"
        );
        assert_eq!(compose(&[], &meta()).unwrap_err(), "no trace to compose");

        // One usable run beside an empty one composes, and says one was
        // dropped.
        let mut blocked = run(vec![]);
        blocked.status = Status::Blocked;
        blocked.detail = "the page stopped changing after 3 steps".to_string();
        let composed = compose(
            &[run(vec![click("hint", PAGE_A, PAGE_B)]), blocked],
            &meta(),
        )
        .unwrap();
        assert_eq!(composed.warnings.len(), 1, "{:?}", composed.warnings);
        assert!(
            composed.warnings[0].contains("no step"),
            "{:?}",
            composed.warnings
        );
    }

    #[test]
    fn a_run_that_did_not_reach_done_is_composed_with_a_warning() {
        let mut partial = run(vec![click("hint", PAGE_A, PAGE_B)]);
        partial.status = Status::Budget;
        partial.detail = "stopped after 40 steps without reaching DONE".to_string();
        let composed = compose(&[partial], &meta()).unwrap();
        assert_eq!(composed.warnings.len(), 1, "{:?}", composed.warnings);
        assert!(
            composed.warnings[0].contains("ended budget rather than done"),
            "{:?}",
            composed.warnings
        );
        assert_eq!(
            composed.flow.steps.len(),
            2,
            "the recorded path is still composed"
        );
    }

    #[test]
    fn traces_that_start_elsewhere_are_refused() {
        let mut elsewhere = run(vec![click("hint", PAGE_A, PAGE_B)]);
        elsewhere.url = "https://other.com/".to_string();
        assert_eq!(
            compose(
                &[run(vec![click("hint", PAGE_A, PAGE_B)]), elsewhere],
                &meta()
            )
            .unwrap_err(),
            "the traces start at different urls, so one flow cannot describe them: \
             https://e.com/login and https://other.com/"
        );
    }

    #[test]
    fn a_variable_repeated_across_runs_is_declared_once() {
        let runs: Vec<Trace> = (0..2)
            .map(|_| {
                run(vec![
                    traced(
                        "fill",
                        Some("textbox containing 'From'"),
                        Some("Zurich"),
                        Some("from"),
                        PAGE_A,
                        PAGE_A,
                    ),
                    traced(
                        "fill",
                        Some("textbox containing 'To'"),
                        Some("London"),
                        Some("to"),
                        PAGE_A,
                        PAGE_A,
                    ),
                    traced(
                        "fill",
                        Some("textbox containing 'From'"),
                        Some("Zurich"),
                        Some("from"),
                        PAGE_A,
                        PAGE_A,
                    ),
                ])
            })
            .collect();
        let composed = compose(&runs, &meta()).unwrap();
        let names: Vec<&str> = composed
            .flow
            .vars
            .iter()
            .map(|var| var.name.as_str())
            .collect();
        assert_eq!(names, ["from", "to"]);
        assert_eq!(composed.flow.vars[1].default.as_deref(), Some("London"));
    }

    #[test]
    fn a_flow_with_no_new_element_carries_no_success_signal_and_says_so() {
        let composed = compose(
            &[run(vec![traced("wait", None, None, None, PAGE_A, PAGE_A)])],
            &meta(),
        )
        .unwrap();
        assert_eq!(composed.flow.success_signal, None);
        assert_eq!(composed.warnings.len(), 1, "{:?}", composed.warnings);
        assert!(
            composed.warnings[0].contains("carries no success signal"),
            "{:?}",
            composed.warnings
        );
        assert_eq!(composed.flow.preconditions.len(), 1);
        assert!(composed.flow.preconditions[0].contains("independently"));
        // An element the final page gained is preferred over one it lost.
        let single = run(vec![click("hint", PAGE_A, PAGE_B)]);
        assert_eq!(
            success_signal(&[&single]).as_deref(),
            Some("page shows 'Select account'")
        );
        let to_c = run(vec![click("hint", PAGE_A, PAGE_C)]);
        assert_eq!(
            success_signal(&[&to_c]).as_deref(),
            Some("page shows 'Code'"),
            "BTreeSet order: the alphabetically first gained name"
        );
        // Two branches compose one signal only when BOTH final pages gained
        // the element: a replay may take either branch.
        let a = run(vec![click("a-hint", PAGE_A, PAGE_C)]);
        let b = run(vec![click("b-hint", PAGE_B, PAGE_C)]);
        assert_eq!(
            success_signal(&[&a, &b]).as_deref(),
            Some("page shows 'Code'"),
            "both final pages gained it; the signal must survive whichever branch ran"
        );
        // No name both runs gained: no signal, and the composition says so.
        let b2 = run(vec![click("b-hint", PAGE_B, PAGE_B)]);
        assert_eq!(success_signal(&[&a, &b2]), None);
    }

    /// A replay's deviation names a step by its path; the revision has to
    /// find that exact step again, nesting included.
    #[test]
    fn a_step_path_finds_the_step_it_names_at_any_depth() {
        let inner = FlowStep {
            action: "click".to_string(),
            ref_hint: Some("button containing 'Inner'".to_string()),
            ..Default::default()
        };
        let conditional = FlowStep {
            action: "conditional".to_string(),
            condition: Some("page shows 'Welcome back'".to_string()),
            then: vec![inner],
            otherwise: vec![FlowStep {
                action: "click".to_string(),
                ref_hint: Some("button containing 'Otherwise'".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut steps = vec![
            FlowStep {
                action: "open".to_string(),
                url: Some(URL.to_string()),
                ..Default::default()
            },
            conditional,
        ];
        assert_eq!(
            locate_mut(&mut steps, "2t.1").unwrap().ref_hint.as_deref(),
            Some("button containing 'Inner'")
        );
        assert_eq!(
            locate_mut(&mut steps, "2o.1").unwrap().ref_hint.as_deref(),
            Some("button containing 'Otherwise'")
        );
        assert_eq!(locate_mut(&mut steps, "1").unwrap().action, "open");
        // A branch letter on the last segment names a branch, not a step;
        // and a path past the end names nothing at all.
        for path in ["2t", "2o", "3", "2t.2", "0", "", "2.1", "x"] {
            assert!(locate_mut(&mut steps, path).is_none(), "{path:?}");
        }
    }

    /// The repair a replay found becomes a branch: the next run chooses
    /// between the recorded step and the discovered one by condition.
    #[test]
    fn a_repair_wraps_the_step_it_replaces() {
        let recorded = FlowStep {
            action: "click".to_string(),
            ref_hint: Some("button containing 'Sign in'".to_string()),
            on_failure: Some(DECIDE_ON_FAILURE.to_string()),
            ..Default::default()
        };
        let repaired = FlowStep {
            action: "click".to_string(),
            ref_hint: Some("button containing 'Continue'".to_string()),
            ..Default::default()
        };
        let wrapped = wrap_with_fallback(&recorded, repaired.clone()).unwrap();
        assert_eq!(wrapped.action, "conditional");
        assert_eq!(wrapped.condition.as_deref(), Some("page shows 'Sign in'"));
        assert_eq!(wrapped.then.len(), 1);
        assert_eq!(
            wrapped.then[0].ref_hint.as_deref(),
            Some("button containing 'Sign in'")
        );
        assert_eq!(
            wrapped.otherwise[0].ref_hint.as_deref(),
            Some("button containing 'Continue'")
        );
        assert!(
            wrapped
                .rationale
                .as_deref()
                .unwrap()
                .contains("re-decided it"),
            "{:?}",
            wrapped.rationale
        );
        // Both branches are marked decidable, so the new branch can be
        // repaired in turn.
        assert!(is_decidable(&wrapped.then[0]));
        assert!(is_decidable(&wrapped.otherwise[0]));

        // A name with an inner apostrophe survives whole: the condition must
        // name the page's own words, not stop at the apostrophe.
        let apostrophe = FlowStep {
            ref_hint: Some("button containing 'Don't have an account?'".to_string()),
            ..Default::default()
        };
        let wrapped = wrap_with_fallback(&apostrophe, repaired.clone()).unwrap();
        assert_eq!(
            wrapped.condition.as_deref(),
            Some("page shows 'Don't have an account?'")
        );

        // A step the condition cannot name is not wrapped: there is no
        // element to ask the page about.
        for hint in [None, Some("button containing Continue")] {
            let unnamed = FlowStep {
                ref_hint: hint.map(ToString::to_string),
                ..recorded.clone()
            };
            assert_eq!(
                wrap_with_fallback(&unnamed, repaired.clone()),
                None,
                "{hint:?}"
            );
        }
        let _ = DECIDE_ON_FAILURE;
    }

    #[test]
    fn a_caller_supplied_name_and_tags_survive_composition() {
        let composed = compose(
            &[run(vec![click("hint", PAGE_A, PAGE_B)])],
            &FlowMeta {
                name: "login-path".to_string(),
                title: "Login path".to_string(),
                tags: vec!["auth".to_string(), "github".to_string()],
            },
        )
        .unwrap();
        assert_eq!(composed.flow.name, "login-path");
        assert_eq!(composed.flow.title, "Login path");
        assert_eq!(composed.flow.tags, ["auth", "github"]);
    }
}
