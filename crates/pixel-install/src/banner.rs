// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The human-facing exit of an interactive `pixel install`: the summary a
//! person at a terminal reads, with the tour of what to visit next.
//! `--json` and a piped stdout keep the machine-readable report — this
//! banner is never the contract an agent parses.

use crate::install::{CheckStatus, InstallReport, InstallSummary};

/// "pixel" in the blocky face the landing page opens with. Seven rows tall
/// and 50 columns wide, so it survives every terminal the install runs in.
const LOGO: &str = "
░█████████  ░██                      ░██
░██     ░██                          ░██
░██     ░██ ░██░██    ░██  ░███████  ░██
░█████████  ░██ ░██  ░██  ░██    ░██ ░██
░██         ░██  ░█████   ░█████████ ░██
░██         ░██ ░██  ░██  ░██        ░██
░██         ░██░██    ░██  ░███████  ░██";

/// The homepage titles, quoted as the tribute they are: the site is built
/// like the tool, down to its section headings.
pub const TOUR: &[(&str, &str)] = &[
    (
        "https://pixel-cli.dev",
        "the landing page — \u{201c}Every session starts cold.\u{201d}",
    ),
    ("https://pixel-cli.dev/docs", "the manual, hook by hook"),
    (
        "https://pixel-cli.dev/vs/jev",
        "the classify benchmark, caveats included",
    ),
];

/// The commands a finished install points at, with what each one does.
const NEXT: &[(&str, &str)] = &[
    ("pixel doctor . --fix", "verify a repository end to end"),
    (
        "pixel install --repo .",
        "add project-local enforcement to a clone",
    ),
];

/// One styled run of text inside the summary: the text, and the SGR code
/// that paints it (`None` renders it plain). Widths are counted on the
/// plain text, so the box aligns with color on or off.
type Span = (String, Option<&'static str>);

fn styled(text: impl Into<String>, code: &'static str) -> Span {
    (text.into(), Some(code))
}

fn plain(text: impl Into<String>) -> Span {
    (text.into(), None)
}

/// Wrap `text` in the SGR `code` when `color` is on; pass it through plain
/// otherwise. [`NO_COLOR`] is resolved by the caller.
fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The status symbol and SGR code of one install step: ✓ green, • yellow,
/// ✗ red — the same shapes `pixel doctor` renders.
fn symbol(status: CheckStatus) -> (&'static str, &'static str) {
    match status {
        CheckStatus::Green => ("✓", "32"),
        CheckStatus::Yellow => ("•", "33"),
        CheckStatus::Red => ("✗", "31"),
    }
}

fn render_logo(color: bool) -> String {
    let mut out = String::new();
    for line in LOGO.trim().lines() {
        out.push_str(&paint(color, "1;32", line));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// The display width of a line: the plain text of its spans, counted in
/// characters so the border stays square on the multibyte symbols.
fn line_width(line: &[Span]) -> usize {
    line.iter()
        .map(|(text, _)| text.chars().count())
        .sum::<usize>()
}

/// Render one line's spans, painting each styled run when `color` is on.
fn render_line(color: bool, line: &[Span]) -> String {
    line.iter()
        .map(|(text, code)| match code {
            Some(code) => paint(color, code, text),
            None => text.clone(),
        })
        .collect()
}

/// Wrap the lines in a rounded box the way gum frames a summary: `╭─╮`
/// corners, two spaces of air on each side, every row padded to the widest
/// line so the right border is square with color on or off.
fn boxed(color: bool, lines: &[Vec<Span>]) -> String {
    let width = lines.iter().map(|line| line_width(line)).max().unwrap_or(0);
    let mut out = String::new();
    out.push('╭');
    out.push_str(&"─".repeat(width + 2));
    out.push_str("╮\n");
    for line in lines {
        out.push_str("│ ");
        out.push_str(&render_line(color, line));
        out.push_str(&" ".repeat(width - line_width(line)));
        out.push_str(" │\n");
    }
    out.push('╰');
    out.push_str(&"─".repeat(width + 2));
    out.push_str("╯\n");
    out
}

/// The widest line the summary box allows before a word-wrap: wide enough
/// for a full hint, narrow enough for the terminals an install runs in.
const SUMMARY_WIDTH: usize = 100;

/// Add one word to the flattened list: the first word of a line just sets
/// the indent, every later one carries the exact run of spaces that
/// separated it from the previous word.
fn push_word(
    words: &mut Vec<(String, Option<&'static str>)>,
    indent: &mut usize,
    pending: &mut usize,
    mut word: String,
    code: Option<&'static str>,
) {
    if word.is_empty() {
        return;
    }
    if words.is_empty() {
        *indent = *pending;
    } else {
        word.insert_str(0, &" ".repeat(*pending));
    }
    words.push((word, code));
    *pending = 0;
}

/// Word-wrap one styled line at [`SUMMARY_WIDTH`], keeping each word's
/// color. Leading plain spaces indent the first row; `hang` prefixes every
/// continuation row so wrapped text lines up under itself. A run of two or
/// more spaces rides with the word that follows it, so an aligned column
/// survives; a word longer than the width is never broken, so a long path
/// may still widen the box.
fn wrap_line(line: &[Span], hang: &str) -> Vec<Vec<Span>> {
    let mut words: Vec<(String, Option<&'static str>)> = Vec::new();
    let mut indent = 0usize;
    let mut pending = 0usize;
    for (text, code) in line {
        let mut word = String::new();
        for character in text.chars() {
            if character == ' ' {
                push_word(
                    &mut words,
                    &mut indent,
                    &mut pending,
                    std::mem::take(&mut word),
                    *code,
                );
                pending += 1;
            } else {
                word.push(character);
            }
        }
        push_word(&mut words, &mut indent, &mut pending, word, *code);
    }
    let hang_width = hang.chars().count();
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut length = 0usize;
    for (word, code) in words {
        let word_width = word.chars().count();
        let limit = if rows.is_empty() {
            SUMMARY_WIDTH - indent
        } else {
            SUMMARY_WIDTH
        };
        if length + word_width > limit && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            length = hang_width;
        }
        // A word starting a row drops the gap it carried across the wrap:
        // the hang prefix takes its place.
        let word = if current.is_empty() {
            word.trim_start().to_string()
        } else {
            word
        };
        let word_width = word.chars().count();
        current.push((word, code));
        length += word_width;
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows.into_iter()
        .enumerate()
        .map(|(position, row)| {
            let mut line = vec![plain(if position == 0 {
                " ".repeat(indent)
            } else {
                hang.to_string()
            })];
            line.extend(row);
            line
        })
        .collect()
}

/// The headline glyph and text of the summary: ✓ installed, • a dry run,
/// ✗ a run that finished with red steps.
fn headline(report: &InstallReport) -> (&'static str, &'static str, &'static str) {
    if report.dry_run {
        (
            "•",
            "33",
            "dry run — nothing was written; every step below is what WOULD happen.",
        )
    } else if report.ok {
        (
            "✓",
            "32",
            "installed. Every agent on this machine now starts with the map.",
        )
    } else {
        (
            "✗",
            "31",
            "finished with red steps — they are named below, and re-running is safe.",
        )
    }
}

/// The `4 green · 1 yellow · 0 red` line, each count in its own color.
fn counts_line(summary: &InstallSummary) -> Vec<Span> {
    let InstallSummary { green, yellow, red } = summary;
    let mut line: Vec<Span> = Vec::new();
    for (position, (count, word, code)) in [
        (green, "green", "32"),
        (yellow, "yellow", "33"),
        (red, "red", "31"),
    ]
    .into_iter()
    .enumerate()
    {
        if position > 0 {
            line.push(styled(" · ", "2"));
        }
        line.push(styled(format!("{count} {word}"), code));
    }
    line
}

/// Render the interactive install header before any work begins.
pub fn render_start(color: bool) -> String {
    let mut out = render_logo(color);
    out.push_str(&paint(
        color,
        "2",
        &format!(
            "v{} — rewiring the agents on this machine",
            env!("CARGO_PKG_VERSION")
        ),
    ));
    out.push_str("\n\n");
    out
}

/// Render the interactive install completion summary after work finishes.
/// `color` gates every SGR sequence; the layout, symbols and links render
/// identically without it.
pub fn render_result(report: &InstallReport, color: bool) -> String {
    let InstallReport {
        version: _,
        ok: _,
        executable_path,
        home: _,
        dry_run: _,
        steps,
        summary,
    } = report;
    // Each line carries the indent its continuation rows hang from, so a
    // wrapped summary lines up under its own first word.
    let mut lines: Vec<(Vec<Span>, &str)> = Vec::new();
    lines.push((Vec::new(), ""));
    let (mark, code, headline) = headline(report);
    lines.push((
        vec![styled(mark, code), plain("  "), styled(headline, "1")],
        "   ",
    ));
    lines.push((vec![plain("  "), styled(executable_path, "2")], "  "));
    lines.push((Vec::new(), ""));
    for step in steps {
        let (mark, code) = symbol(step.status);
        lines.push((
            vec![styled(mark, code), plain(format!(" {}", step.summary))],
            "  ",
        ));
        if let Some(detail) = step.detail.as_deref().filter(|detail| !detail.is_empty()) {
            lines.push((vec![plain("      "), styled(detail, "2")], "      "));
        }
    }
    lines.push((Vec::new(), ""));
    lines.push((counts_line(summary), "  "));
    if report.ok && !report.dry_run {
        lines.push((Vec::new(), ""));
        lines.push((vec![styled("Next", "1")], "  "));
        let command_width = NEXT
            .iter()
            .map(|(command, _)| command.chars().count())
            .max()
            .unwrap_or(0);
        for (command, description) in NEXT.iter() {
            let gap = " ".repeat(command_width - command.chars().count() + 3);
            lines.push((
                vec![
                    plain("    "),
                    styled("›", "2"),
                    plain(" "),
                    styled(*command, "92"),
                    plain(gap),
                    styled(*description, "2"),
                ],
                "      ",
            ));
        }
    }
    lines.push((Vec::new(), ""));
    let lines: Vec<Vec<Span>> = lines
        .into_iter()
        .flat_map(|(line, hang)| wrap_line(&line, hang))
        .collect();
    let mut out = boxed(color, &lines);
    out.push_str(&paint(
        color,
        "2",
        "\nMade by hand, down to the landing page:\n",
    ));
    for (url, note) in TOUR {
        out.push_str("  ");
        out.push_str(&paint(color, "92", url));
        out.push_str("  —  ");
        out.push_str(note);
        out.push('\n');
    }
    out
}

/// Render the complete interactive install output as one banner.
pub fn render(report: &InstallReport, color: bool) -> String {
    let mut out = render_start(color);
    out.push_str(&render_result(report, color));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{InstallStep, InstallSummary};

    fn report(status: CheckStatus, summary: &str) -> InstallReport {
        InstallReport {
            version: "v1".into(),
            ok: status == CheckStatus::Green,
            executable_path: "/usr/local/bin/pixel".into(),
            home: "/home/example".into(),
            dry_run: false,
            steps: vec![InstallStep {
                id: "agent-prompt".into(),
                status,
                summary: summary.into(),
                detail: None,
            }],
            summary: match status {
                CheckStatus::Green => InstallSummary {
                    green: 1,
                    yellow: 0,
                    red: 0,
                },
                CheckStatus::Yellow => InstallSummary {
                    green: 0,
                    yellow: 1,
                    red: 0,
                },
                CheckStatus::Red => InstallSummary {
                    green: 0,
                    yellow: 0,
                    red: 1,
                },
            },
        }
    }

    /// Every box row must be the same display width as its neighbours, so
    /// the right border is square whatever the longest line is.
    fn box_rows(banner: &str) -> Vec<&str> {
        banner
            .lines()
            .filter(|line| line.starts_with('│'))
            .collect()
    }

    /// Drop the SGR sequences so colored output can be width-checked the
    /// same way as plain.
    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn a_nonempty_step_detail_renders_below_its_summary() {
        let mut detailed = report(CheckStatus::Yellow, "legacy wrapper left");
        detailed.steps[0].detail = Some("removed=old clean=[]".into());
        let banner = render_result(&detailed, false);
        assert!(banner.contains("• legacy wrapper left"), "{banner}");
        assert!(banner.contains("      removed=old clean=[]"), "{banner}");

        let mut empty = report(CheckStatus::Green, "verified");
        empty.steps[0].detail = Some(String::new());
        let plain = render_result(&empty, false);
        assert!(!plain.contains("      \n"), "{plain}");
    }

    #[test]
    fn every_box_row_is_padded_to_the_same_display_width() {
        let banner = render_result(&report(CheckStatus::Green, "verified"), false);
        let rows = box_rows(&banner);
        assert!(rows.len() > 2, "{banner}");
        let width = rows[0].chars().count();
        assert!(
            rows.iter().all(|row| row.chars().count() == width),
            "{banner}"
        );
        let top = banner.lines().find(|line| line.starts_with('╭')).unwrap();
        assert_eq!(top.chars().count(), width, "{banner}");
        let bottom = banner.lines().find(|line| line.starts_with('╰')).unwrap();
        assert_eq!(bottom.chars().count(), width, "{banner}");
        // The breathing-room rows are part of the box: losing them would
        // cramp the summary against its border.
        assert!(
            rows.iter()
                .any(|row| row.trim_matches(|c: char| c == '│' || c == ' ').is_empty()),
            "{banner}"
        );
    }

    #[test]
    fn the_box_border_survives_color_because_widths_are_counted_plain() {
        let colored = render_result(&report(CheckStatus::Green, "verified"), true);
        let rows = box_rows(&colored);
        let width = rows.first().map(|row| strip_ansi(row).chars().count());
        assert!(
            rows.iter()
                .all(|row| strip_ansi(row).chars().count() == width.unwrap_or(0)),
            "{colored}"
        );
    }

    #[test]
    fn the_banner_names_every_step_with_its_status_symbol_and_counts() {
        let banner = render_result(
            &report(CheckStatus::Green, "verified agent-prompt.md"),
            false,
        );
        assert!(banner.contains("✓ verified agent-prompt.md"), "{banner}");
        // The counts are one joined line, not three loose numbers: the
        // separators are part of the contract, and none sits before the
        // first count.
        let counts_row = box_rows(&banner)
            .into_iter()
            .find(|row| row.contains("green"))
            .expect("counts row in {banner}");
        assert!(
            counts_row.starts_with("│ 1 green · 0 yellow · 0 red"),
            "{counts_row}"
        );
        assert!(counts_row.ends_with(" │"), "{counts_row}");
        assert!(banner.contains("installed."), "{banner}");
        assert!(banner.contains("/usr/local/bin/pixel"), "{banner}");

        let yellow = render_result(&report(CheckStatus::Yellow, "legacy wrapper left"), false);
        assert!(yellow.contains("• legacy wrapper left"), "{yellow}");
        assert!(yellow.contains("1 yellow"), "{yellow}");
        assert!(!yellow.contains("installed."), "{yellow}");

        let red = render_result(&report(CheckStatus::Red, "could not write hooks"), false);
        assert!(red.contains("✗ could not write hooks"), "{red}");
        assert!(red.contains("1 red"), "{red}");
        assert!(red.contains("re-running is safe"), "{red}");
    }

    #[test]
    fn the_next_commands_align_into_one_column_with_their_descriptions() {
        let banner = render_result(&report(CheckStatus::Green, "verified"), false);
        let doctor = banner
            .lines()
            .find(|line| line.contains("pixel doctor . --fix"))
            .expect("doctor command in {banner}");
        let install = banner
            .lines()
            .find(|line| line.contains("pixel install --repo ."))
            .expect("install command in {banner}");
        // "pixel install --repo ." is 4 characters longer, so its gap is
        // exactly 3 smaller: both descriptions start on the same column.
        assert!(doctor.contains("--fix     verify a repository"), "{doctor}");
        assert!(
            install.contains("--repo .   add project-local"),
            "{install}"
        );
    }

    #[test]
    fn the_banner_carries_the_tour() {
        let banner = render_result(&report(CheckStatus::Green, "verified"), false);
        for (url, _) in TOUR {
            assert!(banner.contains(url), "{banner}");
        }
        assert!(banner.contains("pixel-cli.dev"), "{banner}");
        assert!(
            banner.contains("Made by hand, down to the landing page:"),
            "{banner}"
        );
        assert!(!banner.contains('"'), "{banner}");
    }

    #[test]
    fn a_dry_run_prints_what_would_happen_and_no_next_commands() {
        let mut dry = report(CheckStatus::Green, "verified agent-prompt.md");
        dry.dry_run = true;
        dry.ok = true;
        let banner = render_result(&dry, false);
        assert!(banner.contains("dry run — nothing was written"), "{banner}");
        assert!(!banner.contains("Next"), "{banner}");
        assert!(!banner.contains("installed."), "{banner}");
    }

    #[test]
    fn a_summary_longer_than_the_width_wraps_and_hangs_under_itself() {
        let long = "claude guard not installed: the personal ingest hook also rewrites shell calls, narrow its matcher to tools other than Bash, then rerun the install so the composed guard can sit beside it".to_string();
        let mut wrapped = report(CheckStatus::Yellow, &long);
        wrapped.ok = false;
        let banner = render_result(&wrapped, false);
        let width = SUMMARY_WIDTH + 4; // `│ ` + text + pad + ` │`
        assert!(
            box_rows(&banner)
                .iter()
                .all(|row| strip_ansi(row).chars().count() <= width),
            "{banner}"
        );
        // The wrap continues the summary under its own text: the row after
        // the one holding the glyph starts with the two-space hang, never
        // with another glyph.
        let rows = box_rows(&banner);
        let glyph_row = rows
            .iter()
            .position(|row| row.contains("• claude guard not installed"))
            .expect("glyph row in {banner}");
        // The first row of a step starts at the border with the glyph; the
        // hang belongs only to the continuation rows.
        assert!(rows[glyph_row].starts_with("│ • claude guard"), "{banner}");
        assert!(
            rows[glyph_row + 1].starts_with("│   to tools other than"),
            "{}\n---\n{}",
            rows[glyph_row + 1],
            banner
        );
    }

    #[test]
    fn a_detail_line_wraps_at_the_width_not_past_it() {
        // 6 spaces of indent + 40 + 1 + 55 = 102: past the 100-column
        // width, so the second word must start its own row.
        let first = "f".repeat(40);
        let second = "s".repeat(55);
        let mut bounded = report(CheckStatus::Green, "verified");
        bounded.steps[0].detail = Some(format!("{first} {second}"));
        let banner = render_result(&bounded, false);
        let second_row = box_rows(&banner)
            .into_iter()
            .find(|row| row.contains(&second))
            .expect("second word row in {banner}");
        assert!(!second_row.contains(&first), "{second_row}");
    }

    #[test]
    fn a_line_exactly_at_the_width_stays_on_one_row() {
        let first = "a".repeat(60);
        let second = "b".repeat(37); // glyph + space + 60 + 1 + 37 = 100
        let mut exact = report(CheckStatus::Green, &format!("{first} {second}"));
        exact.ok = true;
        let banner = render_result(&exact, false);
        let row = box_rows(&banner)
            .into_iter()
            .find(|row| row.contains(&first))
            .expect("summary row in {banner}");
        assert!(row.contains(&second), "{row}");
    }

    #[test]
    fn a_word_longer_than_the_width_is_never_broken() {
        let path = format!("/{}", "d".repeat(SUMMARY_WIDTH + 20));
        let mut long_path = report(CheckStatus::Green, "verified");
        long_path.steps[0].detail = Some(path.clone());
        let banner = render_result(&long_path, false);
        assert!(banner.contains(&path), "{banner}");
    }

    #[test]
    fn color_off_never_emits_an_escape_sequence_and_color_on_does() {
        let plain = render_result(&report(CheckStatus::Green, "verified"), false);
        assert!(!plain.contains('\x1b'), "{plain}");
        let colored = render_result(&report(CheckStatus::Green, "verified"), true);
        assert!(colored.contains("\x1b[1m"), "{colored}");
        assert!(colored.contains("\x1b[32m✓\x1b[0m"), "{colored}");
        // The site's green, not cyan: URLs and next commands carry the
        // landing page's accent color.
        assert!(
            colored.contains("\x1b[92mhttps://pixel-cli.dev"),
            "{colored}"
        );
        assert!(render_start(true).contains("\x1b[1;32m░"), "{colored}");
        assert!(colored.ends_with('\n'), "{colored}");
    }

    #[test]
    fn the_start_banner_carries_the_logo_version_and_what_is_starting() {
        let start = render_start(false);
        assert!(start.starts_with("░█████████"), "{start}");
        assert!(start.contains("░███████"), "{start}");
        assert!(
            start.contains(&format!("v{} — rewiring", env!("CARGO_PKG_VERSION"))),
            "{start}"
        );
        assert!(!start.contains("installed."), "{start}");
        assert!(!start.contains("pixel-cli.dev"), "{start}");
    }

    #[test]
    fn render_places_the_start_banner_before_the_completion_summary() {
        let banner = render(&report(CheckStatus::Green, "verified"), false);
        assert!(
            banner.find("░█████████") < banner.find("installed."),
            "{banner}"
        );
    }

    #[test]
    fn the_logo_spells_the_name_in_seven_rows() {
        let logo_rows = LOGO.trim().lines().count();
        assert_eq!(logo_rows, 7, "{LOGO}");
        assert!(LOGO.contains("░█████████  ░██"), "{LOGO}");
        assert!(LOGO.contains("░███████  ░██"), "{LOGO}");
        assert!(
            LOGO.trim().lines().all(|row| row.ends_with("░██")),
            "{LOGO}"
        );
    }
}
