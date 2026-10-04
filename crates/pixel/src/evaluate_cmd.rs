// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel evaluate` — bounded predicate evaluation with a witness.
//!
//! The daemon owns the answer; this module owns the argument surface, the
//! rendering and the exit code.
//!
//! Two rules shape the output. First, the scope is inseparable from the
//! verdict: the human form leads with the summary's first sentence, which
//! names the snapshot, the relation, the tiers and the traversal, so a
//! screenshot of the answer cannot be read as a claim about the running
//! program. Second, `0` means *evaluated*, not *true* — an `absent` and an
//! `unknown` both exit `0`, while a usage error exits `2` and a technical
//! failure `3`, so a script can tell "the answer is no" from "the question
//! was refused".

use std::path::PathBuf;

use pixel_daemon::api::Request;
use pixel_proto::evaluate as wire;
use serde_json::Value;

/// What `pixel evaluate path` was asked.
#[derive(Debug, Clone)]
pub struct EvaluateOptions {
    pub from: String,
    pub to: String,
    pub traversal: String,
    pub tiers: String,
    pub max_depth: Option<u32>,
    pub time_budget_ms: Option<u64>,
    pub scope: Option<String>,
    pub at_snapshot: bool,
    pub path: PathBuf,
    pub json: bool,
}

/// Run the evaluation, print its envelope, and report the exit code its
/// outcome maps to.
///
/// The code is returned rather than handed to `std::process::exit` here:
/// exiting inside the command would skip the action log its caller writes,
/// so `pixel evaluate` would be the one command absent from the journal.
/// The caller owns the exit, which also makes the three-way contract
/// (0 evaluated / 2 usage / 3 technical) assertable from a test without a
/// subprocess.
pub fn run(opts: EvaluateOptions) -> i32 {
    let json = opts.json;
    let asked = Asked {
        from: opts.from.clone(),
        to: opts.to.clone(),
    };
    let output = match evaluate(opts) {
        Ok(output) => output,
        Err(error) => wire::Output::Error(error),
    };
    print_output(&output, json, &asked);
    output.exit_code()
}

/// What the caller typed for each symbol argument. The envelope names the
/// *flag* a reason belongs to, which is what a machine re-asks with; a
/// person also needs to see the value that failed, and only the CLI knows
/// it.
struct Asked {
    from: String,
    to: String,
}

impl Asked {
    fn value_of(&self, argument: &str) -> Option<&str> {
        match argument {
            "--from" => Some(&self.from),
            "--to" => Some(&self.to),
            _ => None,
        }
    }
}

/// Ask the daemon, and turn a transport or usage failure into the typed
/// error the contract prescribes.
fn evaluate(opts: EvaluateOptions) -> Result<wire::Output, wire::ErrorEnvelope> {
    // Validated here as well as in the daemon so a bad flag is a usage
    // error before a socket is opened, and so the message names the flag.
    if !matches!(opts.traversal.as_str(), "callees" | "callers") {
        return Err(usage(
            "--traversal",
            format!(
                "unknown --traversal {:?} (callees | callers)",
                opts.traversal
            ),
        ));
    }
    if !matches!(opts.tiers.as_str(), "exact" | "exact,probable") {
        return Err(usage(
            "--tiers",
            format!("unknown --tiers {:?} (exact | exact,probable)", opts.tiers),
        ));
    }
    let request = Request::Evaluate {
        from: opts.from,
        to: opts.to,
        traversal: Some(opts.traversal),
        tiers: Some(opts.tiers),
        max_depth: opts.max_depth,
        time_budget_ms: opts.time_budget_ms,
        scope: opts.scope,
        at_snapshot: opts.at_snapshot,
    };
    let data = crate::execute(&opts.path, request, false).map_err(technical)?;
    parse(data)
}

/// Read the daemon's answer back into the contract type. A payload that
/// does not deserialize is a technical failure, not an evaluation: the
/// command would rather exit 3 than print a half-understood verdict.
fn parse(data: Value) -> Result<wire::Output, wire::ErrorEnvelope> {
    serde_json::from_value::<wire::Output>(data).map_err(|e| technical(e.to_string()))
}

fn usage(argument: &str, message: String) -> wire::ErrorEnvelope {
    wire::ErrorEnvelope {
        code: wire::ErrorKind::InvalidArgument,
        message,
        argument: Some(argument.to_string()),
    }
}

fn technical(message: String) -> wire::ErrorEnvelope {
    wire::ErrorEnvelope {
        code: wire::ErrorKind::Internal,
        message,
        argument: None,
    }
}

/// `--json` prints the one contract object on stdout, whatever the outcome,
/// so a caller parses one shape and never scrapes prose. The human form
/// prints the summary and the evidence on stdout, diagnostics on stderr.
fn print_output(output: &wire::Output, json: bool, asked: &Asked) {
    if json {
        match serde_json::to_string_pretty(output) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("evaluate: cannot render answer: {error}"),
        }
        return;
    }
    match output {
        wire::Output::Evaluation(envelope) => print_human(envelope, asked),
        wire::Output::Error(error) => {
            let message = &error.message;
            match &error.argument {
                Some(argument) => eprintln!("evaluate: {argument}: {message}"),
                None => eprintln!("evaluate: {message}"),
            }
        }
    }
}

/// The human rendering: the verdict with its scope first, then the witness
/// or the way out.
fn print_human(envelope: &wire::EvaluationEnvelope, asked: &Asked) {
    println!("{}", envelope.summary);
    match &envelope.outcome {
        wire::Outcome::Established { witness } => print_witness(witness),
        wire::Outcome::AbsentInSnapshot => {}
        wire::Outcome::Unknown {
            reason,
            next_actions,
        } => {
            print_offending_value(reason, asked);
            // The summary already names the first action; printing it again
            // would read as two instructions for one problem.
            for action in next_actions.iter().skip(1) {
                println!("  next: {}", action.phrase());
            }
            print_candidates(reason);
        }
    }
}

/// The value behind the flag a reason blames, so the reader does not have
/// to scroll back to their own command line.
fn print_offending_value(reason: &wire::Reason, asked: &Asked) {
    let argument = match reason {
        wire::Reason::AmbiguousSymbol { argument, .. }
        | wire::Reason::SymbolNotFound { argument }
        | wire::Reason::SymbolOutsideIndex { argument, .. } => argument,
        _ => return,
    };
    if let Some(value) = asked.value_of(argument) {
        println!("  {argument} was: {value}");
    }
}

/// Each hop as a line a reader can open: the call site is where the edge is
/// written, which is what makes the witness checkable rather than merely
/// asserted.
fn print_witness(witness: &wire::Witness) {
    match witness {
        wire::Witness::Path { edges, .. } => {
            for edge in edges {
                let step = edge.traversal_step;
                let from = &edge.from.uid;
                let to = &edge.to.uid;
                let site_path = &edge.edge.site.path;
                let site_line = edge.edge.site.line;
                let tier = edge.edge.tier.as_str();
                println!("  {step}. {from} → {to}  [{tier}] {site_path}:{site_line}");
            }
        }
        wire::Witness::Identity { symbol, .. } => {
            let uid = &symbol.uid;
            let path = &symbol.path;
            let line = symbol.lines[0];
            println!("  0. {uid}  {path}:{line}");
        }
    }
}

/// Ambiguity is only actionable if the caller can see what to re-ask with,
/// so the candidates are printed with the uids that resolve them.
fn print_candidates(reason: &wire::Reason) {
    let wire::Reason::AmbiguousSymbol { candidates, .. } = reason else {
        return;
    };
    for candidate in candidates {
        let uid = &candidate.uid;
        let kind = &candidate.kind;
        let path = &candidate.path;
        let line = candidate.line;
        println!("  candidate: {uid}  [{kind}] {path}:{line}");
    }
    // A full list is a bounded list: the lookup stops at the cap, so more
    // symbols may share the name. Saying so is cheaper than letting a
    // reader assume the list is everything.
    if candidate_list_is_capped(candidates.len()) {
        let cap = pixel_daemon::evaluate::CANDIDATE_CAP;
        println!("  (list capped at {cap}; more symbols may share this name)");
    }
}

/// Whether a candidate list of `count` entries has to be read as truncated.
///
/// The lookup asks the store for at most `CANDIDATE_CAP` rows, so a list
/// that long is the one case where the caller cannot tell "these are all of
/// them" from "these are the first of them". Exactly at the cap counts as
/// truncated: that is the length a truncated list has. Reading it the other
/// way round would stay silent on the only list that needs the warning and
/// print it on every list that does not.
fn candidate_list_is_capped(count: usize) -> bool {
    count >= pixel_daemon::evaluate::CANDIDATE_CAP
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope names the *flag* that failed; only the CLI still holds
    /// what the caller typed for it. Losing either mapping prints a reason
    /// with no value next to it, which is the one thing this indirection
    /// exists to prevent.
    #[test]
    fn each_symbol_flag_should_map_back_to_the_value_the_caller_typed() {
        let asked = Asked {
            from: "handleRequest".to_string(),
            to: "writeAudit".to_string(),
        };
        assert_eq!(asked.value_of("--from"), Some("handleRequest"));
        assert_eq!(asked.value_of("--to"), Some("writeAudit"));
        assert_eq!(
            asked.value_of("--traversal"),
            None,
            "only the two symbol flags carry a value the reason can quote"
        );
        assert_eq!(asked.value_of(""), None);
    }

    /// The cap warning is a claim about completeness, so it must fire on
    /// exactly the lists that are truncated: at the cap and above, never
    /// below it.
    #[test]
    fn only_a_list_at_or_above_the_lookup_cap_should_be_called_truncated() {
        let cap = pixel_daemon::evaluate::CANDIDATE_CAP;
        assert!(!candidate_list_is_capped(0));
        assert!(!candidate_list_is_capped(1));
        assert!(
            !candidate_list_is_capped(cap - 1),
            "one short of the cap is a complete list"
        );
        assert!(
            candidate_list_is_capped(cap),
            "a list exactly as long as the lookup limit is where truncation hides"
        );
        assert!(candidate_list_is_capped(cap + 1));
    }
}
