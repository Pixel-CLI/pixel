// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

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
    /// The guarded logical lines (from P2·3), each `(line_number, trimmed_text)`.
    /// Content-anchored, so they stay valid — and preferred over the fixed
    /// span — when the file moves. Empty when no crux was extracted.
    pub crux: Vec<(u32, String)>,
    /// True when `snippet` stops before `end_line` (a line or byte cap), so
    /// an L2 rendering declares the excerpt condensed instead of passing it
    /// off as the whole body.
    #[serde(default)]
    pub snippet_cut: bool,
}

/// Output layer controlling how much detail to include, ordered by cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Names + locations only.
    L0,
    /// + signatures.
    L1,
    /// + snippets.
    L2,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::L0 => "L0",
            Layer::L1 => "L1",
            Layer::L2 => "L2",
        }
    }
}

/// Outcome of [`fit_items`]: the rendered text and the layer each input
/// item got, `None` for an item the budget could not hold even as a name.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemsFit {
    pub text: String,
    pub layers: Vec<Option<Layer>>,
}

impl ItemsFit {
    /// Number of input items rendered at no layer at all.
    pub fn omitted(&self) -> usize {
        self.layers.iter().filter(|layer| layer.is_none()).count()
    }
}

/// The declared basis for every budget fit in this crate: token counts are
/// a bytes/4 heuristic, NOT a real tokenizer. Responses that present a
/// budget fit should carry this string (e.g. as `budget_basis`) so the
/// approximation is declared rather than passed off as an exact token cap.
pub const BUDGET_BASIS: &str = "bytes/4 estimate (not a real tokenizer)";

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
    if layer == Layer::L2 && item.snippet_cut {
        // The excerpt holds whole lines only, so its last one is complete.
        let shown = u32::try_from(item.snippet.lines().count()).unwrap_or(u32::MAX);
        if shown == 0 {
            let _ = writeln!(
                out,
                "    … body not shown (excerpt cap); full body: {}:{}-{}",
                item.path, item.start_line, item.end_line
            );
        } else {
            let last = item.start_line.saturating_add(shown - 1);
            let _ = writeln!(
                out,
                "    … body cut after line {last}; full body: {}:{}-{}",
                item.path, item.start_line, item.end_line
            );
        }
    }
    // P2·3: crux lines are the distilled body — at L2 they annotate the
    // shown body with the logic-bearing lines (`crux:LINE`), and they are
    // content-anchored so they stay right when the file moves. Neighbors
    // rendered at L1 stay signatures-only; the distilled target fallback
    // lives in `render_context` (daemon side), not here.
    if layer == Layer::L2 && !item.crux.is_empty() {
        let crux: Vec<(u32, &str)> = item
            .crux
            .iter()
            .map(|(line, text)| (*line, text.as_str()))
            .collect();
        render_crux(out, &crux);
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

/// Surface crux lines (the guarded logical lines, from P2·3) in context
/// rendering. Each entry is `(line_number, text)`; order is preserved. Crux
/// lines are anchored by content fingerprint — they stay valid even when the
/// file moves — so they replace a budget-clipped body excerpt with the lines
/// that actually carry the guards, mutations and early bails.
///
/// Rendered as an indented `crux ▸` block after the item header. Deterministic:
/// same crux input yields identical output.
pub fn render_crux(out: &mut String, crux: &[(u32, &str)]) {
    use std::fmt::Write;
    for (line, text) in crux {
        // Highlight the logic-bearing line; keep `line` as a retrieval hint
        // (the fingerprint is the true anchor, surfaced by whoever loaded the
        // crux from the graph store).
        let _ = writeln!(out, "    crux:{line} {text}");
    }
}

/// Deterministic, minimal crux scorer mirroring the graph store's heuristic
/// (guards/branches, mutations, early bails) so this crate can render crux
/// from a raw body standalone. The graph store's `extract_crux` is the
/// canonical implementation; callers that already read crux from storage
/// should pass those lines straight to [`render_crux`] instead.
pub fn crux_lines_from_body(body: &str, threshold: i64) -> Vec<(u32, String)> {
    body.lines()
        .enumerate()
        .filter_map(|(idx, raw)| {
            let t = raw.trim();
            if t.is_empty()
                || t.chars().all(|c| c == '{' || c == '}')
                || t.starts_with("//")
                || t.starts_with("#")
                || t.starts_with("\"")
            {
                return None;
            }
            let mut score = 0i64;
            let guard = [
                "if ", "else if", "while ", "for ", "match ", "catch", "when ", "guard", "assert",
                "check", "ensure", "validate",
            ];
            if guard.iter().any(|w| t.contains(w)) {
                score += 3;
            }
            let bail = [
                "return ",
                "break;",
                "continue;",
                "throw ",
                "panic!",
                "unwrap",
                "expect",
                "abort",
                "exit(",
            ];
            if bail.iter().any(|w| t.contains(w)) || t.ends_with('?') {
                score += 3;
            }
            let mut_pats = [
                "=", "return ", "+=", "-=", "*=", "/=", "push", "insert", "remove", "set", "append",
            ];
            if mut_pats.iter().any(|w| t.contains(w)) {
                score += 3;
            }
            if score >= threshold {
                Some(((idx + 1) as u32, t.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Omitted items named in the elision line before the rest is only counted.
const OMITTED_NAMED_LIMIT: usize = 5;

/// The elision line naming the first omitted items, empty when none is.
fn elision_line(omitted: &[ContextItem]) -> String {
    use std::fmt::Write;
    if omitted.is_empty() {
        return String::new();
    }
    let mut line = format!("… {} more items elided (budget): ", omitted.len());
    for (index, item) in omitted.iter().take(OMITTED_NAMED_LIMIT).enumerate() {
        if index > 0 {
            line.push_str(", ");
        }
        let _ = write!(line, "{} {}:{}", item.name, item.path, item.start_line);
    }
    if omitted.len() > OMITTED_NAMED_LIMIT {
        let _ = write!(line, ", +{} more", omitted.len() - OMITTED_NAMED_LIMIT);
    }
    line.push('\n');
    line
}

/// Fit items into a token budget, one layer per item, in priority order.
///
/// Items are in priority order. First every item that fits gets its L0
/// line, stopping at the first that does not, so an identity is never
/// dropped for a lower-priority one; the omitted rest is named in a closing
/// `… N more items elided (budget): name path:line, …` line whose cost is
/// reserved. Then each placed item is raised one layer at a time (all to L1
/// before any to L2), up to `max_layer`, while the whole text stays within
/// the budget: an item too large to raise does not stop the next one.
pub fn fit_items(items: &[ContextItem], budget_tokens: usize, max_layer: Layer) -> ItemsFit {
    let fits = |bytes: usize| bytes.div_ceil(4) <= budget_tokens;
    let rendered: Vec<[String; 3]> = items
        .iter()
        .map(|item| {
            [Layer::L0, Layer::L1, Layer::L2].map(|layer| {
                let mut out = String::new();
                render_item(item, layer, &mut out);
                out
            })
        })
        .collect();
    let mut layers: Vec<Option<Layer>> = vec![None; items.len()];
    let mut used = 0usize;
    for (index, forms) in rendered.iter().enumerate() {
        let marker = elision_line(&items[index + 1..]);
        if !fits(used + forms[0].len() + marker.len()) {
            break;
        }
        used += forms[0].len();
        layers[index] = Some(Layer::L0);
    }
    let placed = layers.iter().take_while(|layer| layer.is_some()).count();
    let mut marker = elision_line(&items[placed..]);
    // Only reachable when not even the first name fits: fall back to the
    // count, then to nothing rather than overrun the budget.
    if !fits(used + marker.len()) {
        marker = format!("… {} more items elided (budget)\n", items.len() - placed);
        if !fits(used + marker.len()) {
            marker.clear();
        }
    }
    for (from, to) in [(Layer::L0, Layer::L1), (Layer::L1, Layer::L2)] {
        if to > max_layer {
            break;
        }
        for (layer, forms) in layers.iter_mut().zip(&rendered) {
            if *layer != Some(from) {
                continue;
            }
            let raised = used - forms[from as usize].len() + forms[to as usize].len();
            if fits(raised + marker.len()) {
                used = raised;
                *layer = Some(to);
            }
        }
    }
    let mut text = String::with_capacity(used + marker.len());
    for (layer, forms) in layers.iter().zip(&rendered) {
        if let Some(layer) = layer {
            text.push_str(&forms[*layer as usize]);
        }
    }
    text.push_str(&marker);
    ItemsFit { text, layers }
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
            crux: Vec::new(),
            snippet_cut: false,
            snippet: (0..snippet_lines)
                .map(|i| format!("    let step_{i} = process(input); // long body line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn tokens_of(parts: &[String]) -> usize {
        estimate_tokens(&parts.concat())
    }

    fn line(item: &ContextItem, layer: Layer) -> String {
        render(std::slice::from_ref(item), layer)
    }

    #[test]
    fn fit_items_should_raise_items_in_priority_order_when_only_some_signatures_fit() {
        let items: Vec<ContextItem> = (0..3)
            .map(|i| item(&format!("handler_{i}"), &format!("src/mod_{i}.rs"), 12))
            .collect();
        // Room for every name plus exactly one signature: the first item gets it.
        let budget = tokens_of(&[
            line(&items[0], Layer::L1),
            line(&items[1], Layer::L0),
            line(&items[2], Layer::L0),
        ]);
        let fit = fit_items(&items, budget, Layer::L2);
        assert_eq!(
            fit.layers,
            vec![Some(Layer::L1), Some(Layer::L0), Some(Layer::L0)]
        );
        assert_eq!(
            fit.text,
            format!(
                "{}{}{}",
                line(&items[0], Layer::L1),
                line(&items[1], Layer::L0),
                line(&items[2], Layer::L0)
            )
        );
        assert_eq!(fit.omitted(), 0);
    }

    #[test]
    fn fit_items_should_keep_several_bodies_when_the_budget_holds_them() {
        let items = vec![item("alpha", "src/a.rs", 3), item("beta", "src/b.rs", 3)];
        let both = tokens_of(&[line(&items[0], Layer::L2), line(&items[1], Layer::L2)]);
        let fit = fit_items(&items, both, Layer::L2);
        assert_eq!(fit.layers, vec![Some(Layer::L2), Some(Layer::L2)]);
        assert!(fit.text.contains("let step_0") && fit.text.matches("let step_0").count() == 2);
        // One body short: the first keeps its body, the second its signature.
        let one = tokens_of(&[line(&items[0], Layer::L2), line(&items[1], Layer::L1)]);
        let fit = fit_items(&items, one, Layer::L2);
        assert_eq!(fit.layers, vec![Some(Layer::L2), Some(Layer::L1)]);
        // A cap at L1 never renders a body, whatever the budget.
        let fit = fit_items(&items, both * 4, Layer::L1);
        assert_eq!(fit.layers, vec![Some(Layer::L1), Some(Layer::L1)]);
        assert!(!fit.text.contains("let step_0"));
        // A cap at L0 renders names only.
        let fit = fit_items(&items, both * 4, Layer::L0);
        assert_eq!(fit.layers, vec![Some(Layer::L0), Some(Layer::L0)]);
    }

    #[test]
    fn fit_items_should_raise_the_next_item_when_one_is_too_large_to_raise() {
        let mut wide = item("wide", "src/w.rs", 2);
        wide.sig = format!("fn wide({})", "arg: Value, ".repeat(40));
        let items = vec![wide, item("small", "src/s.rs", 2)];
        let budget = tokens_of(&[line(&items[0], Layer::L0), line(&items[1], Layer::L1)]);
        let fit = fit_items(&items, budget, Layer::L2);
        assert_eq!(fit.layers, vec![Some(Layer::L0), Some(Layer::L1)]);
    }

    #[test]
    fn fit_items_should_name_what_it_omits_within_the_budget() {
        let items: Vec<ContextItem> = (0..4)
            .map(|i| item(&format!("h{i}"), &format!("src/m{i}.rs"), 2))
            .collect();
        let marker = "… 2 more items elided (budget): h2 src/m2.rs:10, h3 src/m3.rs:10\n";
        let kept = [line(&items[0], Layer::L0), line(&items[1], Layer::L0)];
        let budget = tokens_of(&[kept[0].clone(), kept[1].clone(), marker.to_owned()]);
        // The elision line's room is kept through the raising passes too.
        let fit = fit_items(&items, budget, Layer::L2);
        assert_eq!(
            fit.layers,
            vec![Some(Layer::L0), Some(Layer::L0), None, None]
        );
        assert_eq!(fit.omitted(), 2);
        assert_eq!(fit.text, format!("{}{}{marker}", kept[0], kept[1]));
        assert!(estimate_tokens(&fit.text) <= budget);
    }

    #[test]
    fn fit_items_should_count_beyond_five_named_omissions() {
        let items: Vec<ContextItem> = (0..8)
            .map(|i| item(&format!("h{i}"), &format!("src/m{i}.rs"), 2))
            .collect();
        let named = "… 8 more items elided (budget): h0 src/m0.rs:10, h1 src/m1.rs:10, \
                     h2 src/m2.rs:10, h3 src/m3.rs:10, h4 src/m4.rs:10, +3 more\n";
        let fit = fit_items(&items, estimate_tokens(named), Layer::L2);
        assert_eq!(fit.layers, vec![None; 8]);
        assert_eq!(fit.text, named);
    }

    #[test]
    fn fit_items_should_name_exactly_five_omissions_without_a_remainder() {
        let items: Vec<ContextItem> = (0..5)
            .map(|i| item(&format!("h{i}"), &format!("src/m{i}.rs"), 2))
            .collect();
        let named = "… 5 more items elided (budget): h0 src/m0.rs:10, h1 src/m1.rs:10, \
                     h2 src/m2.rs:10, h3 src/m3.rs:10, h4 src/m4.rs:10\n";
        assert_eq!(
            fit_items(&items, estimate_tokens(named), Layer::L2).text,
            named
        );
    }

    #[test]
    fn fit_items_should_shorten_then_drop_the_elision_line_rather_than_overrun() {
        let items: Vec<ContextItem> = (0..8)
            .map(|i| item(&format!("h{i}"), &format!("src/m{i}.rs"), 2))
            .collect();
        let count = "… 8 more items elided (budget)\n";
        let fit = fit_items(&items, estimate_tokens(count), Layer::L2);
        assert_eq!((fit.text.as_str(), fit.omitted()), (count, 8));
        let fit = fit_items(&items, estimate_tokens(count) - 1, Layer::L2);
        assert_eq!((fit.text.as_str(), fit.omitted()), ("", 8));
    }

    #[test]
    fn fit_items_should_place_an_item_at_the_exact_budget_and_not_one_token_under() {
        // Grow the signature until the L1 line is a whole number of tokens,
        // so one token less cannot hold it.
        let mut exact = item("alpha", "src/a.rs", 2);
        while !line(&exact, Layer::L1).len().is_multiple_of(4) {
            exact.sig.push('x');
        }
        let budget = line(&exact, Layer::L1).len() / 4;
        let items = vec![exact];
        assert_eq!(
            fit_items(&items, budget, Layer::L1).layers,
            vec![Some(Layer::L1)]
        );
        assert_eq!(
            fit_items(&items, budget - 1, Layer::L1).layers,
            vec![Some(Layer::L0)]
        );
    }

    #[test]
    fn fit_items_should_return_nothing_for_no_items() {
        let fit = fit_items(&[], 100, Layer::L2);
        assert_eq!(
            fit,
            ItemsFit {
                text: String::new(),
                layers: Vec::new()
            }
        );
    }

    #[test]
    fn render_should_declare_a_cut_body_and_where_the_full_one_is() {
        let mut cut = item("long_fn", "src/long.rs", 3);
        cut.end_line = 200;
        cut.snippet_cut = true;
        let text = render(std::slice::from_ref(&cut), Layer::L2);
        assert!(
            text.ends_with("    … body cut after line 12; full body: src/long.rs:10-200\n"),
            "{text}"
        );
        // An empty excerpt says the body was not shown, not a line before it.
        let mut empty = cut.clone();
        empty.snippet = String::new();
        assert!(
            render(std::slice::from_ref(&empty), Layer::L2)
                .ends_with("    … body not shown (excerpt cap); full body: src/long.rs:10-200\n")
        );
        // Only a body rendering carries the marker.
        assert!(!render(std::slice::from_ref(&cut), Layer::L1).contains("body cut"));
        cut.snippet_cut = false;
        assert!(!render(std::slice::from_ref(&cut), Layer::L2).contains("body cut"));
    }

    #[test]
    fn crux_rendering_is_deterministic_and_order_preserving() {
        let crux = vec![(2u32, "if cfg.dry_run {"), (4u32, "return 0;")];
        let mut a = String::new();
        let mut b = String::new();
        render_crux(&mut a, &crux);
        render_crux(&mut b, &crux);
        assert_eq!(a, b);
        assert!(a.contains("crux:2 if cfg.dry_run {"));
        assert!(a.contains("crux:4 return 0;"));
        // Line order preserved.
        assert!(a.find("crux:2").unwrap() < a.find("crux:4").unwrap());

        // Body scorer surfaces guard/mutation/bail, skips delimiters/comments.
        let body = "pub fn f() {\n    if x {\n        return 1;\n    }\n    // note\n    let y = 2;\n    y\n}";
        let lines = crux_lines_from_body(body, 3);
        let texts: Vec<&str> = lines.iter().map(|(_, t)| t.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("if x")));
        assert!(texts.iter().any(|t| t.contains("return 1")));
        assert!(!texts.iter().any(|t| *t == "{" || *t == "}"));
    }

    #[test]
    fn crux_lines_from_body_should_score_each_signal_and_skip_noise_lines() {
        let body = [
            "{",
            "// if x = 1",
            "# if x = 1",
            "\"if x = 1\"",
            "",
            "foo()?",
            "if a {",
            "x = 1;",
            "return x",
            "if a { return b; }",
            "plain()",
        ]
        .join("\n");
        let at = |threshold| crux_lines_from_body(&body, threshold);
        let line = |n: u32, t: &str| (n, t.to_string());
        assert_eq!(
            at(0),
            vec![
                line(6, "foo()?"),
                line(7, "if a {"),
                line(8, "x = 1;"),
                line(9, "return x"),
                line(10, "if a { return b; }"),
                line(11, "plain()"),
            ]
        );
        assert_eq!(
            at(3),
            vec![
                line(6, "foo()?"),
                line(7, "if a {"),
                line(8, "x = 1;"),
                line(9, "return x"),
                line(10, "if a { return b; }"),
            ]
        );
        assert_eq!(
            at(6),
            vec![line(9, "return x"), line(10, "if a { return b; }")]
        );
        assert_eq!(at(9), vec![line(10, "if a { return b; }")]);
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
