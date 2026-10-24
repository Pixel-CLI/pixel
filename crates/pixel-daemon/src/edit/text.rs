// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The texts the edit hooks inject (`specs/hooks.md`). Each is bounded to
//! [`BUDGET`] bytes and cut on a line boundary, saying so.

use pixel_graph::edit::{Site, SymbolEdit};
use pixel_graph::shape::ShapeChange;

use super::{Pending, name_of_uid};

/// Most bytes of one injected text.
pub const BUDGET: usize = 1_200;
/// Proved callers a card lists.
const PROVED_ROWS: usize = 8;
/// Likely callers a card lists.
const LIKELY_ROWS: usize = 4;

fn site_line(site: &Site) -> String {
    format!("- {}:{} in {}", site.file, site.line, site.holder)
}

/// `lines` joined, keeping whole lines within [`BUDGET`].
fn bounded(lines: Vec<String>) -> String {
    let mut out = String::new();
    let total = lines.len();
    for (kept, line) in lines.iter().enumerate() {
        if out.len() + line.len() + 1 > BUDGET {
            out.push_str(&format!("(+{} more lines cut)\n", total - kept));
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// `[PIXEL:EDIT]`: what the edit does to the shape of the symbols it touches
/// and who still depends on the old one.
pub fn card(edits: &[&SymbolEdit]) -> String {
    let mut lines = Vec::new();
    for edit in edits {
        let name = &edit.symbol.name;
        let file = &edit.symbol.file;
        let head = match &edit.change {
            ShapeChange::Reshaped { old, new, .. } => {
                format!("`{name}` ({file}) changes shape: `{old}` -> `{new}`")
            }
            ShapeChange::Removed { old, .. } => format!("`{name}` ({file}) is removed: `{old}`"),
            ShapeChange::Renamed { to, .. } => {
                format!("`{name}` ({file}) is renamed to `{}`", name_of_uid(to))
            }
        };
        lines.push(if lines.is_empty() {
            format!("[PIXEL:EDIT] {head}")
        } else {
            head
        });
        if !edit.proved.is_empty() {
            lines.push("Callers on the old shape (proved):".to_owned());
            lines.extend(edit.proved.iter().take(PROVED_ROWS).map(site_line));
            if edit.proved.len() > PROVED_ROWS {
                lines.push(format!("- +{} more", edit.proved.len() - PROVED_ROWS));
            }
        }
        if !edit.likely.is_empty() {
            lines.push("Possible callers (likely, not proved):".to_owned());
            lines.extend(edit.likely.iter().take(LIKELY_ROWS).map(site_line));
        }
        if !edit.tests.is_empty() {
            lines.push(format!("Tests within reach: {}", edit.tests.join(", ")));
        }
        if edit.open_sites > 0 {
            lines.push(format!(
                "Lower bound: {} more call sites of this name could not be resolved.",
                edit.open_sites
            ));
        }
    }
    bounded(lines)
}

fn pending_rows(pending: &[Pending]) -> Vec<String> {
    let mut lines = Vec::new();
    for p in pending {
        let shape = p.new.as_deref().map_or_else(
            || "removed or renamed".to_owned(),
            |new| format!("now `{new}`"),
        );
        lines.push(format!(
            "`{}` ({}) was `{}`, {shape}:",
            p.name, p.file, p.old
        ));
        lines.extend(
            p.sites
                .iter()
                .map(|s| format!("- {}:{} in {}", s.file, s.line, s.holder)),
        );
    }
    lines
}

/// `[PIXEL:STALE-CALLERS]`: callers of a changed shape no edit has touched.
pub fn stale(added: &[Pending]) -> String {
    let mut lines = vec![
        "[PIXEL:STALE-CALLERS] These call sites have not changed since the shape did:".to_owned(),
    ];
    lines.extend(pending_rows(added));
    lines.push("Update them, or check that they still fit.".to_owned());
    bounded(lines)
}

/// The reason Stop is blocked once: the sites still on the old shape.
pub fn block(pending: &[Pending]) -> String {
    let mut lines =
        vec!["[PIXEL:STALE-CALLERS] These call sites still use the old shape:".to_owned()];
    lines.extend(pending_rows(pending));
    lines.push("Update them, or state why they are fine.".to_owned());
    bounded(lines)
}
