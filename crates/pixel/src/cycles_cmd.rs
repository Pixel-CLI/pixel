// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel cycles` — bounded recursion-cycle enumeration with witnesses and
//! explicit coverage.
//!
//! The daemon owns the answer; this module owns the argument surface, the
//! rendering and the exit code.
//!
//! Two rules shape the output. First, the coverage is inseparable from the
//! verdict: the human form leads with whether the enumeration was exhaustive
//! or which budget stopped it, so a screenshot of the answer cannot be read
//! as a complete claim. Second, `0` means *enumerated*, not *safe* — a
//! partial enumeration and a complete one both exit `0`, while a usage error
//! exits `2` and a technical failure `3`.

use std::path::PathBuf;

use pixel_daemon::api::Request;
use serde_json::Value;

/// What `pixel cycles` was asked.
#[derive(Debug, Clone)]
pub struct CyclesOptions {
    pub tiers: String,
    pub max_nodes: Option<u32>,
    pub max_edges: Option<u32>,
    pub time_budget_ms: Option<u64>,
    pub max_components: Option<u32>,
    pub path: PathBuf,
    pub json: bool,
}

/// Run the enumeration, print its result, and report the exit code.
///
/// The code is returned rather than handed to `std::process::exit` here:
/// exiting inside the command would skip the action log its caller writes.
/// The caller owns the exit, which also makes the three-way contract
/// (0 enumerated / 2 usage / 3 technical) assertable from a test without a
/// subprocess.
pub fn run(opts: CyclesOptions) -> i32 {
    let json = opts.json;
    let output = match enumerate(opts) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cycles: {error}");
            return 2;
        }
    };
    print_output(&output, json);
    0
}

/// Ask the daemon, and turn a transport or usage failure into a typed error.
fn enumerate(opts: CyclesOptions) -> Result<Value, String> {
    if !matches!(opts.tiers.as_str(), "exact" | "exact,probable") {
        return Err(format!(
            "unknown --tiers {:?} (exact | exact,probable)",
            opts.tiers
        ));
    }
    let request = Request::Cycles {
        tiers: Some(opts.tiers),
        max_nodes: opts.max_nodes,
        max_edges: opts.max_edges,
        time_budget_ms: opts.time_budget_ms,
        max_components: opts.max_components,
    };
    crate::execute(&opts.path, request, false)
}

/// `--json` prints the one contract object on stdout, whatever the outcome,
/// so a caller parses one shape and never scrapes prose. The human form
/// prints the coverage and the components on stdout, diagnostics on stderr.
fn print_output(data: &Value, json: bool) {
    if json {
        match serde_json::to_string_pretty(data) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("cycles: cannot render answer: {error}"),
        }
        return;
    }
    print_human(data);
}

/// The human rendering: the coverage first, then each component with its
/// witness.
fn print_human(data: &Value) {
    let coverage = data.get("coverage");
    let components = data.get("components").and_then(|c| c.as_array());

    // Coverage first: an incomplete enumeration must never be read as a
    // complete "no cycles" answer.
    if let Some(cov) = coverage {
        let exhausted = cov
            .get("enumeration_exhausted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if exhausted {
            println!("Enumeration exhaustive: all reachable nodes visited.");
        } else if let Some(stopped_by) = cov.get("stopped_by").and_then(|v| v.as_str()) {
            println!("Enumeration INCOMPLETE: stopped by {stopped_by}.");
            println!("  This is NOT a complete 'no cycles' answer.");
        }
        if let Some(unresolved) = cov
            .get("unresolved_same_name_sites")
            .and_then(serde_json::Value::as_u64)
            && unresolved > 0
        {
            println!("  Unresolved call sites: {unresolved}");
        }
    }

    let Some(comps) = components else {
        println!("No cycles found.");
        return;
    };

    if comps.is_empty() {
        println!("No cycles found.");
        return;
    }

    println!("{} cycle(s) found:", comps.len());
    for (i, comp) in comps.iter().enumerate() {
        let id = comp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let members = comp.get("members").and_then(|v| v.as_array());
        let member_count = members.map_or(0, Vec::len);
        println!("  {}. Component {id} ({member_count} members)", i + 1);
        if let Some(witness) = comp.get("witness") {
            print_witness(witness);
        }
    }
}

/// Each hop as a line a reader can open: the call site is where the edge is
/// written, which is what makes the witness checkable rather than merely
/// asserted.
fn print_witness(witness: &Value) {
    let Some(edges) = witness.get("edges").and_then(|e| e.as_array()) else {
        return;
    };
    for edge in edges {
        let step = edge.get("step").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let from = edge
            .get("from")
            .and_then(|f| f.get("uid"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let to = edge
            .get("to")
            .and_then(|t| t.get("uid"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let site = edge.get("edge").and_then(|e| e.get("site"));
        let site_path = site
            .and_then(|s| s.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let site_line = site.and_then(|s| s.get("line")).and_then(serde_json::Value::as_u64).unwrap_or(0);
        let tier = edge
            .get("edge")
            .and_then(|e| e.get("tier"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("    {step}. {from} → {to}  [{tier}] {site_path}:{site_line}");
    }
}
