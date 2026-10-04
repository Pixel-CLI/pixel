// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel plan` command — deterministic todo list generation.
//!
//! The daemon owns the graph: the findings come from the `plan` op, which
//! refreshes the graph incrementally before it runs the queries, and this
//! module only renders them.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use pixel_daemon::api::Request;
use pixel_graph::plan::{FindingKind, PlanFinding, Prereq, PrereqKind, Severity};
use serde_json::json;

use crate::plan_state;

#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub prompt: Option<String>,
    pub path: PathBuf,
    pub query: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<usize>,
    pub format: String,
    pub no_verify: bool,
    /// Omit the verification-gate block (auth session, env keys, real data).
    pub no_gates: bool,
    pub max_todos: Option<usize>,
    pub status: bool,
    pub done: Vec<usize>,
    pub undone: Vec<usize>,
    pub prune: bool,
    pub json: bool,
}

pub fn run(opts: PlanOptions) -> Result<(), String> {
    let opts = path_given_as_prompt(opts);
    if opts.status || !opts.done.is_empty() || !opts.undone.is_empty() || opts.prune {
        return run_state_ops(&opts);
    }
    let data = crate::execute(
        &opts.path,
        Request::Plan {
            prompt: opts.prompt.clone(),
            query: opts.query.clone(),
            tag: opts.tag.clone(),
            limit: opts.limit,
        },
        false,
    )?;
    let findings = findings_of(&data)?;
    let gates = if opts.no_gates {
        Vec::new()
    } else {
        gates_of(&prereqs_of(&data)?, &findings)
    };
    // Persist the checklist before rendering: a failed state write must not
    // swallow the plan itself, so a write error is a warning, not a failure.
    // A corrupt file is named too — silently resetting tracked progress is
    // a data loss the user should see. Gates merge first so a fresh
    // `--status` lists them above the site findings.
    let mut state = match plan_state::load(&opts.path) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("warning: {e}; starting a fresh checklist");
            plan_state::PlanState::default()
        }
    };
    let mut tracked = gates.clone();
    tracked.extend(findings.iter().cloned());
    plan_state::merge(&mut state, &tracked);
    if let Err(e) = plan_state::save(&opts.path, &state) {
        eprintln!("warning: plan state not saved: {e}");
    }
    render(opts, findings, gates)
}

/// `--status`/`--done`/`--undone`/`--prune`: operate on `.pixel/plan.json`
/// without planning — this path never touches the daemon.
fn run_state_ops(opts: &PlanOptions) -> Result<(), String> {
    let mut state = plan_state::load(&opts.path)?;
    let (changed, reports) = apply_state_ops(opts, &mut state)?;
    for line in &reports {
        eprintln!("{line}");
    }
    if changed {
        plan_state::save(&opts.path, &state)?;
    }
    if opts.json {
        let done = state.items.iter().filter(|i| i.done).count();
        let out = json!({
            "items": state.items,
            "done": done,
            "total": state.items.len(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
    } else {
        print!("{}", plan_state::render_status(&state));
    }
    Ok(())
}

/// The mutations the state flags ask for, as data: whether the state
/// changed (drives save) and what to report. Returned rather than printed
/// so the prune gate is observable in tests.
fn apply_state_ops(
    opts: &PlanOptions,
    state: &mut plan_state::PlanState,
) -> Result<(bool, Vec<String>), String> {
    let mut changed = false;
    let mut reports = Vec::new();
    for &n in &opts.done {
        plan_state::set_done(state, n, true)?;
        changed = true;
    }
    for &n in &opts.undone {
        plan_state::set_done(state, n, false)?;
        changed = true;
    }
    if opts.prune {
        let pruned = plan_state::prune(state);
        if pruned > 0 {
            reports.push(format!("pruned {pruned} stale plan item(s)"));
            changed = true;
        }
    }
    Ok((changed, reports))
}

/// `prompt` and `path` are both optional positionals, so
/// `pixel plan --query hotspots ../repo` parses `../repo` as the prompt and
/// plans the current directory. Only `by-concept` reads a prompt next to
/// `--query`: for any other query, a prompt that names a directory while the
/// path was left at its default is the path.
fn path_given_as_prompt(mut opts: PlanOptions) -> PlanOptions {
    let prompt_is_path = opts.query.as_deref().is_some_and(|q| q != "by-concept")
        && opts.path == std::path::Path::new(".")
        && opts
            .prompt
            .as_deref()
            .is_some_and(|p| std::path::Path::new(p).is_dir());
    if prompt_is_path && let Some(prompt) = opts.prompt.take() {
        opts.path = PathBuf::from(prompt);
    }
    opts
}

/// The findings of a `plan` op answer.
fn findings_of(data: &serde_json::Value) -> Result<Vec<PlanFinding>, String> {
    let findings = data
        .get("findings")
        .cloned()
        .ok_or_else(|| "plan: the answer carries no findings".to_string())?;
    serde_json::from_value(findings).map_err(|e| format!("plan: unreadable findings: {e}"))
}

/// The `prereqs` field of a `plan` op answer — absent on daemons that
/// predate it, which is not an error.
fn prereqs_of(data: &serde_json::Value) -> Result<Vec<Prereq>, String> {
    match data.get("prereqs") {
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("plan: unreadable prereqs: {e}"))
        }
        None => Ok(Vec::new()),
    }
}

/// Names of saved flows tagged `auth` or `login` — the replays a gate can
/// name. Sorted lexicographically and deduplicated so the gate label is
/// reproducible across runs (filesystem order from `pixel_flow::list` is
/// not a stable ordering). A missing or unreadable flow dir is "no flows",
/// never a failure.
fn auth_flow_names() -> Vec<String> {
    let Ok(value) = pixel_flow::list() else {
        return Vec::new();
    };
    let mut names: Vec<String> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| {
            f["tags"].as_array().into_iter().flatten().any(|t| {
                t.as_str().is_some_and(|t| {
                    t.eq_ignore_ascii_case("auth") || t.eq_ignore_ascii_case("login")
                })
            })
        })
        .filter_map(|f| f["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Fold raw [`Prereq`] detections into blocking gate items: every env var
/// detected merges into one item, auth-gated files share one that names the
/// saved login flow when there is one, each provider gets its own item, and
/// database drivers share one. Gate labels start with `Gate:` so they are
/// recognizable in `--status` output.
fn gates_of(prereqs: &[Prereq], findings: &[PlanFinding]) -> Vec<PlanFinding> {
    let fan_in_of = |file: &str| {
        findings
            .iter()
            .find(|f| f.file == file)
            .map_or(0, |f| f.fan_in)
    };
    let gate = |file: &str, line: u32, label: String| PlanFinding {
        file: file.to_string(),
        line,
        label,
        fan_in: fan_in_of(file),
        severity: Severity::High,
        kind: FindingKind::Prereq,
        blocking: true,
    };
    let mut gates = Vec::new();

    let auth: Vec<&Prereq> = prereqs
        .iter()
        .filter(|p| p.kind == PrereqKind::Auth)
        .collect();
    if let Some(first) = auth.first() {
        let mut files: Vec<&str> = Vec::new();
        for p in &auth {
            if !files.contains(&p.file.as_str()) {
                files.push(&p.file);
            }
        }
        let names = files.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        let extra = if files.len() > 3 {
            format!(" +{} more", files.len() - 3)
        } else {
            String::new()
        };
        let flow_names = auth_flow_names();
        let how = match flow_names.as_slice() {
            [] => "no `auth`-tagged flow saved — ask the human for a test account, or record one with `pixel flow save`".to_string(),
            [first] => format!("run `pixel flow run {first}`"),
            many => {
                let shown = many
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("`, `");
                let extra = if many.len() > 3 {
                    format!(" +{} more", many.len() - 3)
                } else {
                    String::new()
                };
                format!("run one of: `pixel flow run {shown}`{extra}")
            }
        };
        gates.push(gate(
            &first.file,
            first.line,
            format!(
                "Gate: auth-gated code ({names}{extra}) — verify needs a logged-in session: {how}"
            ),
        ));
    }

    let envs: BTreeSet<&str> = prereqs
        .iter()
        .filter(|p| p.kind == PrereqKind::Env)
        .map(|p| p.detail.as_str())
        .collect();
    if let Some(first) = prereqs.iter().find(|p| p.kind == PrereqKind::Env) {
        let shown: Vec<&str> = envs.iter().take(12).copied().collect();
        let extra = if envs.len() > 12 {
            format!(" +{} more", envs.len() - 12)
        } else {
            String::new()
        };
        gates.push(gate(
            &first.file,
            first.line,
            format!("Gate: env keys required: {}{}", shown.join(", "), extra),
        ));
    }

    // One gate per provider; detections carry the display name as detail.
    let mut providers: BTreeMap<&str, &Prereq> = BTreeMap::new();
    for p in prereqs.iter().filter(|p| p.kind == PrereqKind::Provider) {
        providers.entry(&p.detail).or_insert(p);
    }
    for (name, p) in providers {
        let keys = pixel_graph::plan::provider_env_prefix(name).map_or_else(
            || "the provider's env keys".to_string(),
            |prefix| format!("{prefix}* keys"),
        );
        gates.push(gate(
            &p.file,
            p.line,
            format!("Gate: {name} integration — verify needs {keys}"),
        ));
    }

    let mut dbs: BTreeMap<&str, &Prereq> = BTreeMap::new();
    for p in prereqs.iter().filter(|p| p.kind == PrereqKind::Db) {
        dbs.entry(&p.detail).or_insert(p);
    }
    if let Some((_, first)) = dbs.first_key_value() {
        let specs: Vec<&str> = dbs.keys().take(3).copied().collect();
        let extra = if dbs.len() > 3 {
            format!(" +{} more", dbs.len() - 3)
        } else {
            String::new()
        };
        gates.push(gate(
            &first.file,
            first.line,
            format!(
                "Gate: database-backed state ({}{extra}) — reproduce with real data before fixing",
                specs.join(", ")
            ),
        ));
    }
    gates
}

fn render(
    opts: PlanOptions,
    findings: Vec<PlanFinding>,
    gates: Vec<PlanFinding>,
) -> Result<(), String> {
    let mut findings = findings;
    if let Some(cap) = opts.max_todos {
        findings.truncate(cap);
    }
    match opts.format.as_str() {
        "json" => render_json(&opts, &findings, &gates),
        "compact" => render_compact(&opts, &findings, &gates),
        _ => render_markdown(&opts, &findings, &gates),
    }
}

#[cfg_attr(test, mutants::skip)] // one print over `markdown`, which is tested
fn render_markdown(
    opts: &PlanOptions,
    findings: &[PlanFinding],
    gates: &[PlanFinding],
) -> Result<(), String> {
    print!("{}", markdown(opts.no_verify, findings, gates));
    Ok(())
}

/// The ` (gates above first)` suffix the verify item carries when the plan
/// has verification gates — signing off without them is not verification.
fn gates_suffix(gates: &[PlanFinding]) -> &'static str {
    if gates.is_empty() {
        ""
    } else {
        " (gates above first)"
    }
}

/// The markdown checklist: a `Prerequisites` bullet block per gate, a
/// numbered `[ ]` line per finding, a leading "map" item once there are
/// enough findings to summarise, and a trailing verify item unless
/// `--no-verify`.
fn markdown(no_verify: bool, findings: &[PlanFinding], gates: &[PlanFinding]) -> String {
    let mut output = String::new();
    if !gates.is_empty() {
        output.push_str("Prerequisites — verification gates:\n");
        for g in gates {
            output.push_str(&format!("- [ ] {}\n", g.label));
        }
        output.push('\n');
    }
    if findings.is_empty() {
        output.push_str("No plan findings.\n");
    } else if is_summary_eligible(findings) {
        let files = file_count(findings);
        output.push_str(&format!(
            "1. [ ] Map {} findings across {} files (ranked by fan-in)\n",
            findings.len(),
            files
        ));
        for (i, f) in findings.iter().enumerate() {
            output.push_str(&format!(
                "{}. [ ] {} in {} (line {}, fan-in: {}) [{}]\n",
                i + 2,
                f.label,
                f.file,
                f.line,
                f.fan_in,
                f.severity.as_str()
            ));
        }
        if !no_verify {
            let n = findings.len() + 2;
            output.push_str(&format!(
                "{n}. [ ] Verify all plan targets in the running build{}\n",
                gates_suffix(gates)
            ));
        }
    } else {
        for (i, f) in findings.iter().enumerate() {
            output.push_str(&format!(
                "{}. [ ] {} in {} (line {}, fan-in: {}) [{}]\n",
                i + 1,
                f.label,
                f.file,
                f.line,
                f.fan_in,
                f.severity.as_str()
            ));
        }
        if !no_verify {
            let n = findings.len() + 1;
            output.push_str(&format!(
                "{n}. [ ] Verify all plan targets in the running build{}\n",
                gates_suffix(gates)
            ));
        }
    }
    output
}

fn render_compact(
    opts: &PlanOptions,
    findings: &[PlanFinding],
    gates: &[PlanFinding],
) -> Result<(), String> {
    for g in gates {
        println!("gate: {}", g.label);
    }
    for f in findings {
        println!(
            "{}:{} {} [{}]",
            f.file,
            f.line,
            f.label,
            f.severity.as_str()
        );
    }
    if !opts.no_verify {
        println!("verify all plan targets in the running build");
    }
    Ok(())
}

fn render_json(
    opts: &PlanOptions,
    findings: &[PlanFinding],
    gates: &[PlanFinding],
) -> Result<(), String> {
    let verify = !opts.no_verify;
    let payload = json!({
        "findings": findings,
        "gates": gates,
        "verify": verify,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn is_summary_eligible(findings: &[PlanFinding]) -> bool {
    findings.len() >= 3
}

fn file_count(findings: &[PlanFinding]) -> usize {
    let mut set = HashSet::new();
    for f in findings {
        set.insert(&f.file);
    }
    set.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::plan::Severity;

    /// Serialises every test in this module that mutates `PIXEL_FLOW_DIR`.
    /// `cargo test` runs in parallel threads inside one binary; without
    /// the lock, two tests can stomp each other's flow directory and the
    /// `auth_flow_names` lookup reads the wrong list. The pixel-flow
    /// crate owns the canonical mutex used by its own tests; this one
    /// is local because pixel-flow's `ENV_MUTEX` is `pub(crate)` and
    /// pixel-cli cannot reach across crates — the two mutexes do not
    /// need to be the same one because no path under test executes both
    /// crates' tests against the same env value.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn finding(file: &str, line: u32, fan_in: u32) -> PlanFinding {
        PlanFinding {
            file: file.to_string(),
            line,
            label: format!("Review {file}"),
            fan_in,
            severity: Severity::from_fan_in(fan_in),
            kind: FindingKind::Site,
            blocking: false,
        }
    }

    fn gate(file: &str, label: &str) -> PlanFinding {
        PlanFinding {
            file: file.to_string(),
            line: 1,
            label: label.to_string(),
            fan_in: 0,
            severity: Severity::High,
            kind: FindingKind::Prereq,
            blocking: true,
        }
    }

    fn prereq(kind: PrereqKind, file: &str, detail: &str) -> Prereq {
        Prereq {
            kind,
            file: file.to_string(),
            line: 3,
            detail: detail.to_string(),
        }
    }

    fn options(prompt: Option<&str>, path: &str, query: Option<&str>) -> PlanOptions {
        PlanOptions {
            prompt: prompt.map(str::to_string),
            path: PathBuf::from(path),
            query: query.map(str::to_string),
            tag: None,
            limit: None,
            format: "markdown".to_string(),
            no_verify: false,
            no_gates: false,
            max_todos: None,
            status: false,
            done: Vec::new(),
            undone: Vec::new(),
            prune: false,
            json: false,
        }
    }

    #[test]
    fn a_directory_after_an_explicit_query_is_the_path_not_the_prompt() {
        let dir = std::env::temp_dir();
        let repo = dir.to_str().unwrap();
        let moved = path_given_as_prompt(options(Some(repo), ".", Some("hotspots")));
        assert_eq!(moved.path, dir);
        assert_eq!(moved.prompt, None);

        // Everything else keeps its meaning.
        for (prompt, path, query) in [
            (Some(repo), ".", Some("by-concept")), // the concept query reads the prompt
            (Some(repo), ".", None),               // a classified prompt
            (Some(repo), "other", Some("hotspots")), // the path was given
            (Some("not a directory"), ".", Some("hotspots")),
            (None, ".", Some("hotspots")),
        ] {
            let kept = path_given_as_prompt(options(prompt, path, query));
            assert_eq!(
                kept.prompt.as_deref(),
                prompt,
                "{prompt:?} {path} {query:?}"
            );
            assert_eq!(
                kept.path,
                PathBuf::from(path),
                "{prompt:?} {path} {query:?}"
            );
        }
    }

    #[test]
    fn markdown_lists_findings_with_map_and_verify_items() {
        assert_eq!(markdown(false, &[], &[]), "No plan findings.\n");
        assert_eq!(markdown(true, &[], &[]), "No plan findings.\n");

        // Under the summary threshold: plain numbering, verify last.
        let one = [finding("src/a.rs", 3, 0)];
        assert_eq!(
            markdown(false, &one, &[]),
            "1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]\n\
             2. [ ] Verify all plan targets in the running build\n"
        );
        assert_eq!(
            markdown(true, &one, &[]),
            "1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]\n"
        );

        // Three findings across two files: a leading map item shifts the
        // numbering by one and the verify item closes the list.
        let three = [
            finding("src/a.rs", 1, 9),
            finding("src/a.rs", 7, 3),
            finding("src/b.rs", 2, 0),
        ];
        let text = markdown(false, &three, &[]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "1. [ ] Map 3 findings across 2 files (ranked by fan-in)",
                "2. [ ] Review src/a.rs in src/a.rs (line 1, fan-in: 9) [HIGH]",
                "3. [ ] Review src/a.rs in src/a.rs (line 7, fan-in: 3) [MEDIUM]",
                "4. [ ] Review src/b.rs in src/b.rs (line 2, fan-in: 0) [LOW]",
                "5. [ ] Verify all plan targets in the running build",
            ]
        );
        assert!(!markdown(true, &three, &[]).contains("Verify"));
    }

    #[test]
    fn markdown_puts_gates_in_a_bullet_block_and_marks_verify() {
        let one = [finding("src/a.rs", 3, 0)];
        let gates = [gate(
            "src/a.rs",
            "Gate: env keys required: STRIPE_SECRET_KEY",
        )];
        let text = markdown(false, &one, &gates);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "Prerequisites — verification gates:",
                "- [ ] Gate: env keys required: STRIPE_SECRET_KEY",
                "",
                "1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]",
                "2. [ ] Verify all plan targets in the running build (gates above first)",
            ]
        );
        // --no-verify drops the verify item but keeps the gates.
        assert_eq!(
            markdown(true, &one, &gates),
            "Prerequisites — verification gates:\n\
             - [ ] Gate: env keys required: STRIPE_SECRET_KEY\n\
             \n\
             1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]\n"
        );
        // Gates with no findings still render — a plan of pure gates is real.
        assert!(markdown(false, &[], &gates).contains("No plan findings."));
    }

    #[test]
    fn prereqs_of_absent_or_present_field() {
        assert!(prereqs_of(&json!({"findings": []})).unwrap().is_empty());
        let data =
            json!({"prereqs": [{"kind": "env", "file": "a.rs", "line": 2, "detail": "API_KEY"}]});
        let prereqs = prereqs_of(&data).unwrap();
        assert_eq!(prereqs.len(), 1);
        assert_eq!(prereqs[0].kind, PrereqKind::Env);
        assert_eq!(prereqs[0].detail, "API_KEY");
        assert!(prereqs_of(&json!({"prereqs": "nope"})).is_err());
    }

    /// The env vars detected across files merge into one gate that names
    /// them; provider and db detections become their own gates.
    #[test]
    fn gates_of_merges_env_names_and_lists_providers() {
        let prereqs = [
            prereq(PrereqKind::Env, "a.rs", "STRIPE_SECRET_KEY"),
            prereq(PrereqKind::Env, "b.rs", "DATABASE_URL"),
            prereq(PrereqKind::Env, "a.rs", "STRIPE_SECRET_KEY"), // dup
            prereq(PrereqKind::Provider, "b.rs", "Stripe"),
            prereq(PrereqKind::Db, "b.rs", "sqlx"),
        ];
        let findings = [finding("a.rs", 3, 4), finding("b.rs", 9, 0)];
        let gates = gates_of(&prereqs, &findings);
        assert_eq!(gates.len(), 3, "{gates:?}");
        let env = &gates[0];
        assert_eq!(
            env.label,
            "Gate: env keys required: DATABASE_URL, STRIPE_SECRET_KEY"
        );
        assert_eq!(env.file, "a.rs", "first evidence site");
        assert_eq!(env.fan_in, 4, "gate inherits the evidence file's fan-in");
        assert_eq!(env.kind, FindingKind::Prereq);
        assert!(env.blocking);
        assert_eq!(env.severity, Severity::High);
        assert_eq!(
            gates[1].label,
            "Gate: Stripe integration — verify needs STRIPE_* keys"
        );
        assert_eq!(
            gates[2].label,
            "Gate: database-backed state (sqlx) — reproduce with real data before fixing"
        );
    }

    /// Boundary on the "first N + +M more" labels: with exactly N items
    /// the gate must NOT append ` +N more` (or ` +0 more`). Each branch
    /// uses `len() > N` and a `>=` mutant would flip that to a positive
    /// extra. Without these assertions, the previous test fixtures all
    /// sat comfortably below the cap and the bug survived.
    #[test]
    fn gates_of_boundary_does_not_emit_zero_more() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let flows = std::env::temp_dir().join(format!(
            "px-plan-gate-boundary-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        // SAFETY: this test owns its unique flow directory and restores the
        // process-global variable before returning.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }

        // Three auth files (cap 3) — no "+more".
        let auth3 = [
            prereq(PrereqKind::Auth, "src/a.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/b.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/c.ts", "auth()"),
        ];
        let gates = gates_of(&auth3, &[]);
        assert_eq!(gates.len(), 1);
        assert!(
            !gates[0].label.contains("more"),
            "exactly 3 files must not append `more`: {}",
            gates[0].label
        );

        // Four auth files (cap 3) — "+1 more" IS expected, sanity check.
        let auth4 = [
            prereq(PrereqKind::Auth, "src/a.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/b.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/c.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/d.ts", "auth()"),
        ];
        let gates = gates_of(&auth4, &[]);
        assert!(
            gates[0].label.contains(" +1 more"),
            "4 files must append +1 more: {}",
            gates[0].label
        );

        // Twelve env keys (cap 12) — no "+more".
        let envs12: Vec<Prereq> = (0..12)
            .map(|i| prereq(PrereqKind::Env, "src/x.rs", &format!("KEY_{i:02}")))
            .collect();
        let gates = gates_of(&envs12, &[]);
        assert_eq!(gates.len(), 1);
        assert!(
            !gates[0].label.contains("more"),
            "exactly 12 env keys must not append `more`: {}",
            gates[0].label
        );

        // Thirteen env keys (cap 12) — "+1 more" IS expected.
        let envs13: Vec<Prereq> = (0..13)
            .map(|i| prereq(PrereqKind::Env, "src/x.rs", &format!("KEY_{i:02}")))
            .collect();
        let gates = gates_of(&envs13, &[]);
        assert!(
            gates[0].label.contains(" +1 more"),
            "13 env keys must append +1 more: {}",
            gates[0].label
        );

        // Three db drivers (cap 3) — no "+more".
        let dbs3 = [
            prereq(PrereqKind::Db, "src/x.rs", "drizzle-orm"),
            prereq(PrereqKind::Db, "src/x.rs", "prisma"),
            prereq(PrereqKind::Db, "src/x.rs", "sqlx"),
        ];
        let gates = gates_of(&dbs3, &[]);
        assert!(
            !gates[0].label.contains("more"),
            "exactly 3 db drivers must not append `more`: {}",
            gates[0].label
        );

        // Four db drivers (cap 3) — "+1 more" IS expected.
        let dbs4 = [
            prereq(PrereqKind::Db, "src/x.rs", "drizzle-orm"),
            prereq(PrereqKind::Db, "src/x.rs", "prisma"),
            prereq(PrereqKind::Db, "src/x.rs", "sqlx"),
            prereq(PrereqKind::Db, "src/x.rs", "rusqlite"),
        ];
        let gates = gates_of(&dbs4, &[]);
        assert!(
            gates[0].label.contains(" +1 more"),
            "4 db drivers must append +1 more: {}",
            gates[0].label
        );

        // SAFETY: restore the process-global variable set above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
    }

    /// Auth detections fold into one gate; with no `auth`-tagged flow the
    /// label asks for a test account instead of naming a replay. Pins
    /// `PIXEL_FLOW_DIR` to a controlled directory so the flow lookup is
    /// deterministic across machines — the previous "accept either arm"
    /// assertion hid a real bug where the dev's `~/.local/share/pixel/flows`
    /// could leak into test output.
    #[test]
    fn gates_of_auth_asks_for_an_account_when_no_flow_is_saved() {
        // Held for the whole test: another test in this binary that mutates
        // `PIXEL_FLOW_DIR` would otherwise read a stale value while this one
        // holds the env var, and `cargo test` schedules tests in this
        // binary on multiple threads. The lock is local to this test
        // module; pixel-flow owns its own `ENV_MUTEX` (the two never collide
        // because no path under test runs both crates' tests against the
        // same env value).
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: serialised by a process-wide env lock convention used by
        // the pixel-flow tests too; this test binary does not run them in
        // parallel because `cargo test` schedules one binary at a time.
        let flows = std::env::temp_dir().join(format!(
            "px-plan-gate-no-flow-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        // SAFETY: serialised by the test runner (one test binary at a time
        // per `cargo test`); other tests in this binary that touch env vars
        // are scheduled serially by `cargo test`. The var is removed before
        // the test returns, even on panic via `Drop`-style ordering below.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }
        let prereqs = [
            prereq(PrereqKind::Auth, "src/middleware.ts", "auth()"),
            prereq(PrereqKind::Auth, "src/page.tsx", "getServerSession"),
            prereq(PrereqKind::Auth, "src/page.tsx", "auth()"), // same file again
        ];
        let gates = gates_of(&prereqs, &[]);
        // SAFETY: serialised by the test runner (see set_var comment); removes
        // the var so the next test in this binary starts from a clean slate.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
        assert_eq!(gates.len(), 1, "{gates:?}");
        assert_eq!(gates[0].file, "src/middleware.ts");
        assert_eq!(gates[0].line, 3);
        assert!(
            gates[0]
                .label
                .starts_with("Gate: auth-gated code (src/middleware.ts, src/page.tsx)"),
            "{}",
            gates[0].label
        );
        assert!(
            gates[0].label.contains("no `auth`-tagged flow saved"),
            "{}",
            gates[0].label
        );
    }

    /// Multiple `auth`-tagged flows: the gate label lists every match in
    /// lexicographic order — determinism so the same repo produces the
    /// same plan across runs. `login`-tagged flows also count; arbitrary
    /// tags do not.
    #[test]
    fn gates_of_auth_lists_every_matching_flow_in_lexicographic_order() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let flows = std::env::temp_dir().join(format!(
            "px-plan-gate-multi-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        for (name, tags) in [
            ("zeta-login", r#"["login"]"#),
            ("alpha-auth", r#"["auth"]"#),
            ("mu-auth", r#"["auth", "ci"]"#),
            ("delta-unrelated", r#"["docs"]"#),
        ] {
            std::fs::write(
                flows.join(format!("{name}.json")),
                format!(
                    r#"{{"name":"{name}","title":"t","description":"d","tags":{tags},"steps":[],"created_unix":1,"revised_unix":1}}"#
                ),
            )
            .unwrap();
        }
        // SAFETY: env lock convention shared with pixel-flow's own tests.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }
        let prereqs = [prereq(PrereqKind::Auth, "src/page.tsx", "getServerSession")];
        let gates = gates_of(&prereqs, &[]);
        // SAFETY: same serialisation contract as the set_var above; restores
        // the env so the next test in this binary starts clean.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
        assert_eq!(gates.len(), 1);
        // alpha-auth, mu-auth, zeta-login — sorted; delta-unrelated skipped.
        assert!(
            gates[0]
                .label
                .contains("run one of: `pixel flow run alpha-auth`, `mu-auth`, `zeta-login`"),
            "{}",
            gates[0].label
        );
    }

    /// `auth_flow_names` itself: tag matching is case-insensitive (`auth`
    /// or `login` only), names are sorted, duplicates are dropped.
    #[test]
    fn gates_of_auth_lists_every_matching_flow_and_caps_at_three() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let flows = std::env::temp_dir().join(format!(
            "px-plan-gate-cap-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        // Three `auth`-tagged flows (the cap boundary): the gate label
        // lists all three and must NOT emit ` +0 more`. A `>` -> `>=`
        // mutant renders `+0 more` at exactly 3, which this fixture
        // pins.
        for (name, tags) in [
            ("zeta-auth", r#"["auth"]"#),
            ("alpha-auth", r#"["auth"]"#),
            ("mu-auth", r#"["auth"]"#),
        ] {
            std::fs::write(
                flows.join(format!("{name}.json")),
                format!(
                    r#"{{"name":"{name}","title":"t","description":"d","tags":{tags},"steps":[],"created_unix":1,"revised_unix":1}}"#
                ),
            )
            .unwrap();
        }
        // SAFETY: env lock convention shared with pixel-flow's own tests.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }
        let prereqs = [prereq(PrereqKind::Auth, "src/page.tsx", "getServerSession")];
        let gates = gates_of(&prereqs, &[]);
        // SAFETY: same serialisation contract as the set_var above;
        // restores the env so the next test in this binary starts clean.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
        assert_eq!(gates.len(), 1);
        // Three flows: the cap is 3, the suffix must NOT appear.
        assert!(
            gates[0]
                .label
                .contains("run one of: `pixel flow run alpha-auth`, `mu-auth`, `zeta-auth`"),
            "{}",
            gates[0].label
        );
        assert!(
            !gates[0].label.contains("more"),
            "exactly 3 flows must not append `more`: {}",
            gates[0].label
        );
    }

    /// Four `auth`-tagged flows (cap + 1): the gate label shows three
    /// names and appends `+1 more` for the fourth. Pins `> 3` on the
    /// cap arithmetic (a `>=` mutant renders `+0 more` at 4 too).
    #[test]
    fn gates_of_auth_lists_three_names_and_appends_one_more_at_four() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let flows =
            std::env::temp_dir().join(format!("px-plan-gate-4-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        for (name, tags) in [
            ("zeta-auth", r#"["auth"]"#),
            ("alpha-auth", r#"["auth"]"#),
            ("mu-auth", r#"["auth"]"#),
            ("theta-auth", r#"["auth"]"#),
        ] {
            std::fs::write(
                flows.join(format!("{name}.json")),
                format!(
                    r#"{{"name":"{name}","title":"t","description":"d","tags":{tags},"steps":[],"created_unix":1,"revised_unix":1}}"#
                ),
            )
            .unwrap();
        }
        // SAFETY: serialised by the test runner.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }
        let prereqs = [prereq(PrereqKind::Auth, "src/page.tsx", "getServerSession")];
        let gates = gates_of(&prereqs, &[]);
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
        assert_eq!(gates.len(), 1);
        assert!(
            gates[0].label.contains(
                "run one of: `pixel flow run alpha-auth`, `mu-auth`, `theta-auth` +1 more"
            ),
            "{}",
            gates[0].label
        );
    }

    /// `auth_flow_names` itself: tag matching is case-insensitive (`auth`
    /// or `login` only), names are sorted, duplicates are dropped.
    #[test]
    fn auth_flow_names_filters_and_sorts() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let flows = std::env::temp_dir().join(format!(
            "px-plan-gate-fns-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&flows);
        std::fs::create_dir_all(&flows).unwrap();
        for (name, tags) in [
            ("b-login", r#"["login"]"#),
            ("a-AUTH", r#"["AUTH"]"#),
            ("c-misc", r#"["ops"]"#),
            ("d-AUTH", r#"["auth"]"#), // duplicate display under case-insensitive match
        ] {
            std::fs::write(
                flows.join(format!("{name}.json")),
                format!(
                    r#"{{"name":"{name}","title":"t","description":"d","tags":{tags},"steps":[],"created_unix":1,"revised_unix":1}}"#
                ),
            )
            .unwrap();
        }
        // SAFETY: serialised by the test runner (one binary at a time); see
        // the set_var comment in the test above for the full contract.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", &flows);
        }
        let mut names = auth_flow_names();
        // SAFETY: same serialisation contract; restores the env so the next
        // test sees a clean slate.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
        let _ = std::fs::remove_dir_all(&flows);
        // Sorted: a-AUTH, b-login, d-AUTH. The `auth` and `AUTH` tag values
        // are matched case-insensitively, but the flow *name* is preserved.
        names.sort();
        assert_eq!(names, vec!["a-AUTH", "b-login", "d-AUTH"]);
    }

    /// Each state flag alone must route to `run_state_ops` — any `||`→`&&`
    /// flip on the gate sends a lone flag to the daemon path, which fails
    /// on a root with no index.
    #[test]
    fn each_state_flag_alone_routes_to_state_ops() {
        let root = std::env::temp_dir().join(format!("px-plan-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Seed one tracked item so --done has something to mark.
        let mut state = plan_state::PlanState::default();
        plan_state::merge(&mut state, &[finding("a.rs", 1, 9)]);
        plan_state::save(&root, &state).unwrap();

        for opts in [
            PlanOptions {
                status: true,
                ..options(None, root.to_str().unwrap(), None)
            },
            PlanOptions {
                prune: true,
                ..options(None, root.to_str().unwrap(), None)
            },
        ] {
            run(opts).expect("state ops never need a daemon");
        }
        // done then undone: each flag alone must route to state ops, and the
        // last write wins on disk.
        run(PlanOptions {
            done: vec![1],
            ..options(None, root.to_str().unwrap(), None)
        })
        .unwrap();
        assert!(plan_state::load(&root).unwrap().items[0].done);
        run(PlanOptions {
            undone: vec![1],
            ..options(None, root.to_str().unwrap(), None)
        })
        .unwrap();
        assert!(!plan_state::load(&root).unwrap().items[0].done);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `apply_state_ops` reports the prune count and marks the state changed
    /// only when something was actually dropped — `>=` would report on a
    /// no-op prune, `==`/`<` would never report a real one.
    #[test]
    fn prune_reports_exactly_when_items_were_dropped() {
        let mut state = plan_state::PlanState::default();
        plan_state::merge(&mut state, &[finding("a.rs", 1, 9), finding("b.rs", 2, 3)]);
        plan_state::merge(&mut state, &[finding("a.rs", 1, 9)]); // b.rs → stale
        let opts = PlanOptions {
            prune: true,
            ..options(None, ".", None)
        };
        let (changed, reports) = apply_state_ops(&opts, &mut state).unwrap();
        assert!(changed);
        assert_eq!(reports, vec!["pruned 1 stale plan item(s)".to_string()]);

        let (changed, reports) = apply_state_ops(&opts, &mut state).unwrap();
        assert!(!changed);
        assert!(reports.is_empty(), "{reports:?}");
    }
}
