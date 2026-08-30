// Portions derived from marjoballabani/hypergrep (MIT) — see NOTICE.
//! Semantic compression of code-context items for AI agents.
//!
//! Instead of dumping raw source, render structured, layered representations
//! that carry the information agents need in far fewer tokens.
//!
//! Layers:
//!   L0 — names + locations only (~10-15 tokens/item)
//!   L1 — + signatures (~30-60 tokens/item)
//!   L2 — + full snippets (~200-800 tokens/item)

use serde::{Deserialize, Serialize};

/// One unit of code context (a symbol, match, or region) to render.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextItem {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub sig: String,
    pub snippet: String,
}

/// Output layer controlling how much detail to include.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layer {
    /// Names + locations only.
    L0,
    /// + signatures.
    L1,
    /// + snippets.
    L2,
}

impl Layer {
    /// The next-cheaper layer, if any (L2 → L1 → L0).
    fn degrade(self) -> Option<Layer> {
        match self {
            Layer::L2 => Some(Layer::L1),
            Layer::L1 => Some(Layer::L0),
            Layer::L0 => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::L0 => "L0",
            Layer::L1 => "L1",
            Layer::L2 => "L2",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    pub text: String,
    pub layer: Layer,
    pub elided_items: usize,
}

/// The declared basis for every budget fit in this crate: token counts are
/// a bytes/4 heuristic, NOT a real tokenizer. Responses that present a
/// budget fit should carry this string (e.g. as `budget_basis`) so the
/// approximation is declared rather than passed off as an exact token cap.
pub const BUDGET_BASIS: &str = "chars/4 estimate (not a real tokenizer)";

/// Rough token estimation: ~4 bytes per token (GPT/Claude average), ceiling.
/// See [`BUDGET_BASIS`] — this is a heuristic, and anything surfacing its
/// output as a "token" count must declare that.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Render one item at the given layer. Deterministic, compact, agent-friendly.
fn render_item(item: &ContextItem, layer: Layer, out: &mut String) {
    use std::fmt::Write;
    // L0: `path:start-end kind name`
    let _ = write!(
        out,
        "{}:{}-{} {} {}",
        item.path, item.start_line, item.end_line, item.kind, item.name
    );
    if matches!(layer, Layer::L1 | Layer::L2) && !item.sig.is_empty() {
        let _ = write!(out, " — {}", item.sig.trim());
    }
    out.push('\n');
    if layer == Layer::L2 && !item.snippet.is_empty() {
        for line in item.snippet.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Render all items at the given layer. Deterministic: preserves input order.
pub fn render(items: &[ContextItem], layer: Layer) -> String {
    let mut out = String::new();
    for item in items {
        render_item(item, layer, &mut out);
    }
    out
}

/// Fit items into a token budget.
///
/// Strategy: try the requested layer; if the rendering exceeds the budget,
/// degrade the layer (L2 → L1 → L0). If even L0 is over budget, greedily
/// truncate items (keeping input order) and append an elision marker line
/// `… N more items elided (budget)`.
pub fn fit_to_budget(items: &[ContextItem], budget_tokens: usize, layer: Layer) -> String {
    fit_to_budget_detailed(items, budget_tokens, layer).text
}

pub fn fit_to_budget_detailed(
    items: &[ContextItem],
    budget_tokens: usize,
    layer: Layer,
) -> FitResult {
    // 1. Degrade layers until one fits (or we bottom out at L0).
    let mut current = layer;
    loop {
        let rendered = render(items, current);
        if estimate_tokens(&rendered) <= budget_tokens {
            return FitResult {
                text: rendered,
                layer: current,
                elided_items: 0,
            };
        }
        match current.degrade() {
            Some(next) => current = next,
            None => break,
        }
    }

    // 2. Still over budget at L0: greedy truncation in input order.
    let mut out = String::new();
    let mut kept = 0usize;
    for item in items {
        let mut candidate = out.clone();
        render_item(item, Layer::L0, &mut candidate);
        // Reserve room for the elision line if not all items will fit.
        let elided = items.len() - (kept + 1);
        let marker = if elided > 0 {
            format!("… {elided} more items elided (budget)\n")
        } else {
            String::new()
        };
        if estimate_tokens(&candidate) + estimate_tokens(&marker) <= budget_tokens {
            out = candidate;
            kept += 1;
        } else {
            break;
        }
    }
    let elided = items.len() - kept;
    if elided > 0 {
        out.push_str(&format!("… {elided} more items elided (budget)\n"));
    }
    FitResult {
        text: out,
        layer: Layer::L0,
        elided_items: elided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, path: &str, snippet_lines: usize) -> ContextItem {
        ContextItem {
            name: name.to_string(),
            kind: "fn".to_string(),
            path: path.to_string(),
            start_line: 10,
            end_line: 10 + snippet_lines as u32,
            sig: format!("fn {name}(input: &str) -> Result<Output, Error>"),
            snippet: (0..snippet_lines)
                .map(|i| format!("    let step_{i} = process(input); // long body line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    #[test]
    fn layer_degradation_under_budget() {
        let items: Vec<ContextItem> = (0..5)
            .map(|i| item(&format!("handler_{i}"), &format!("src/mod_{i}.rs"), 12))
            .collect();

        let l2_tokens = estimate_tokens(&render(&items, Layer::L2));
        let l1_tokens = estimate_tokens(&render(&items, Layer::L1));
        let l0_tokens = estimate_tokens(&render(&items, Layer::L0));
        assert!(l0_tokens < l1_tokens && l1_tokens < l2_tokens);

        // Budget fits L1 but not L2 → degrades to L1 (has sigs, no snippets).
        let fitted = fit_to_budget(&items, l1_tokens, Layer::L2);
        assert_eq!(fitted, render(&items, Layer::L1));
        assert!(fitted.contains("— fn handler_0"));
        assert!(!fitted.contains("let step_0"));

        // Budget below even L0 → truncated L0 with elision marker.
        let tiny = fit_to_budget(&items, l0_tokens - 5, Layer::L2);
        assert!(tiny.contains("more items elided (budget)"));
        assert!(estimate_tokens(&tiny) <= l0_tokens - 5);
        assert!(tiny.starts_with("src/mod_0.rs:"));

        let detailed = fit_to_budget_detailed(&items, l1_tokens, Layer::L2);
        assert_eq!(detailed.layer, Layer::L1);
        assert_eq!(detailed.elided_items, 0);
    }

    #[test]
    fn deterministic_render() {
        let items = vec![item("alpha", "src/a.rs", 3), item("beta", "src/b.rs", 2)];
        let a = render(&items, Layer::L2);
        let b = render(&items, Layer::L2);
        assert_eq!(a, b);
        let expected_first =
            "src/a.rs:10-13 fn alpha — fn alpha(input: &str) -> Result<Output, Error>\n";
        assert!(a.starts_with(expected_first));
        // Order preserved.
        assert!(a.find("alpha").unwrap() < a.find("beta").unwrap());
    }
}
