// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pack-context`: one symbol's source, fitted to a token budget.
//!
//! The lines are the symbol's own (the declaration through its last line)
//! preceded by the doc comment directly above it, numbered the way the brief's
//! region packs number them. A budget counts four bytes to the token; a symbol
//! that does not fit is cut at a line boundary and says how many lines it left
//! out, never claiming to be whole.

use pixel_model::Symbol;

/// At most this many comment lines above the declaration belong to it.
pub const DOC_LINES: usize = 12;

/// The least a budget can be: one line of a declaration is always shown.
pub const MIN_BUDGET_TOKENS: usize = 16;

/// One packed symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed {
    /// 1-based first line shown (the doc comment's, when there is one).
    pub start_line: u32,
    /// 1-based last line shown.
    pub end_line: u32,
    /// The lines shown, numbered, in order.
    pub lines: Vec<(u32, String)>,
    /// Lines of the symbol that did not fit.
    pub cut_lines: usize,
    /// Estimated tokens of what is shown (bytes divided by four, rounded up).
    pub tokens: usize,
}

impl Packed {
    /// The text a person reads: `NNN| line` per line, then the cut note.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (number, line) in &self.lines {
            out.push_str(&format!("{number}| {line}\n"));
        }
        if self.cut_lines > 0 {
            out.push_str(&format!("… {} more line(s) not shown\n", self.cut_lines));
        }
        out
    }
}

/// Packs `symbol` out of `text`, the whole text of its file.
///
/// `None` when the symbol's lines are not in `text` (the file changed since
/// the graph was built).
pub fn pack(symbol: &Symbol, text: &str, budget_tokens: usize) -> Option<Packed> {
    let all: Vec<&str> = text.lines().collect();
    let first = symbol.line.max(1) as usize;
    let last = (symbol.end_line.max(symbol.line)) as usize;
    if first > all.len() {
        return None;
    }
    let last = last.min(all.len());
    let start = doc_start(&all, first);
    let budget = budget_tokens.max(MIN_BUDGET_TOKENS) * 4;
    let mut used = 0;
    let mut lines = Vec::new();
    for number in start..=last {
        let line = all[number - 1];
        let cost = line.len() + 1;
        // The declaration's own first line is always shown.
        if used + cost > budget && number >= first && !lines.is_empty() {
            break;
        }
        used += cost;
        lines.push((number as u32, line.to_owned()));
    }
    let shown_last = lines.last().map_or(start, |(n, _)| *n as usize);
    Some(Packed {
        start_line: start as u32,
        end_line: shown_last as u32,
        cut_lines: last - shown_last,
        tokens: used.div_ceil(4),
        lines,
    })
}

/// The first line of the comment directly above line `first` (1-based), or
/// `first` itself.
fn doc_start(all: &[&str], first: usize) -> usize {
    let mut start = first;
    while start > 1 && first - start < DOC_LINES {
        let above = all[start - 2].trim_start();
        let comment = above.starts_with("///")
            || above.starts_with("//!")
            || above.starts_with("//")
            || above.starts_with('#')
            || above.starts_with("/*")
            || above.starts_with('*')
            || above.starts_with("--")
            || above.starts_with("\"\"\"");
        if !comment {
            break;
        }
        start -= 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use pixel_model::SymbolKind;

    use super::*;

    fn symbol(line: u32, end_line: u32) -> Symbol {
        Symbol {
            uid: "a.rs#f#function".into(),
            file: "a.rs".into(),
            kind: SymbolKind::Function,
            name: "f".into(),
            signature: "fn f()".into(),
            shape: "fn f()".into(),
            line,
            end_line,
        }
    }

    const SOURCE: &str = "// header\n\n/// Does the thing.\n/// Twice.\nfn f() {\n    one();\n    two();\n}\n\nfn g() {}\n";

    #[test]
    fn pack_should_show_the_doc_comment_and_the_whole_body_when_it_fits() {
        let packed = pack(&symbol(5, 8), SOURCE, 200).unwrap();
        assert_eq!(
            (packed.start_line, packed.end_line, packed.cut_lines),
            (3, 8, 0)
        );
        assert_eq!(
            packed.render(),
            "3| /// Does the thing.\n4| /// Twice.\n5| fn f() {\n6|     one();\n7|     two();\n8| }\n"
        );
        assert_eq!(
            packed.tokens,
            packed
                .lines
                .iter()
                .map(|(_, l)| l.len() + 1)
                .sum::<usize>()
                .div_ceil(4)
        );
    }

    #[test]
    fn pack_should_cut_at_a_line_and_say_how_many_lines_it_left_out() {
        let body: String = (1..=12)
            .map(|n| format!("    step_number_{n:02}(argument);\n"))
            .collect();
        let text = format!("/// Doc.\nfn f() {{\n{body}}}\n");
        // 40 tokens = 160 bytes: the doc line, the declaration and a few steps.
        let packed = pack(&symbol(2, 15), &text, 40).unwrap();
        assert_eq!(packed.lines[0].0, 1);
        assert!(packed.cut_lines > 0 && packed.end_line < 15, "{packed:?}");
        assert_eq!(packed.cut_lines, 15 - packed.end_line as usize);
        assert!(packed.tokens <= 40);
        assert!(
            packed
                .render()
                .ends_with(&format!("… {} more line(s) not shown\n", packed.cut_lines))
        );
        // The whole symbol fits in a large budget.
        assert_eq!(pack(&symbol(2, 15), &text, 400).unwrap().cut_lines, 0);
    }

    #[test]
    fn pack_should_always_show_the_declaration_even_when_the_budget_is_tiny() {
        let long = format!("fn f() {{ {} }}\n", "x".repeat(500));
        let packed = pack(&symbol(1, 1), &long, 0).unwrap();
        assert_eq!(packed.lines.len(), 1);
        assert_eq!(packed.cut_lines, 0);
    }

    #[test]
    fn pack_should_not_reach_back_past_the_start_of_the_file_or_twelve_comment_lines() {
        let many: String =
            (0..20).map(|n| format!("// c{n}\n")).collect::<String>() + "fn f() {}\n";
        let packed = pack(&symbol(21, 21), &many, 1000).unwrap();
        assert_eq!(packed.start_line, 21 - DOC_LINES as u32);
        let top = pack(&symbol(2, 2), "// only\nfn f() {}\n", 100).unwrap();
        assert_eq!(top.start_line, 1);
    }

    #[test]
    fn pack_should_refuse_a_symbol_the_file_no_longer_holds() {
        assert_eq!(pack(&symbol(50, 60), SOURCE, 100), None);
        // A span past the end is clipped, not refused.
        let clipped = pack(&symbol(10, 99), SOURCE, 100).unwrap();
        assert_eq!(clipped.end_line, 10);
    }
}
