// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Parser for RuboCop's default (progress/simple) formatter:
//! `path:line:col: C: [Correctable] Cop/Name: message`, one line per
//! offense, then `N files inspected, N offenses detected`; and its JSON
//! formatter (`--format json`, [`parse_json`]), which yields the same
//! offenses.

/// One RuboCop offense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offense {
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// `C` (convention), `W` (warning), `E` (error), `F` (fatal),
    /// `R` (refactor), `I` (info).
    pub severity: String,
    pub cop: String,
    pub message: String,
    pub correctable: bool,
    /// Fixed by `--autocorrect` in this run (`[Corrected]`): reported, but
    /// no longer in the file.
    pub corrected: bool,
}

/// A parsed RuboCop run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub offenses: Vec<Offense>,
    pub files_inspected: Option<u64>,
}

/// Parse one offense line; `None` for anything else.
pub fn parse_line(line: &str) -> Option<Offense> {
    let text = line.trim_end();
    if text.starts_with(char::is_whitespace) {
        return None;
    }
    let (location, rest) = text.split_once(": ")?;
    let (file_line, column) = location.rsplit_once(':')?;
    let (file, line_no) = file_line.rsplit_once(':')?;
    if file.is_empty() || file.contains(char::is_whitespace) {
        return None;
    }
    let line_no: u32 = line_no.parse().ok()?;
    let column: u32 = column.parse().ok()?;
    let (severity, rest) = rest.split_once(": ")?;
    if severity.len() != 1 || !"CWEFRI".contains(severity) {
        return None;
    }
    let (correctable, corrected, rest) = if let Some(rest) = rest.strip_prefix("[Correctable] ") {
        (true, false, rest)
    } else if let Some(rest) = rest.strip_prefix("[Corrected] ") {
        (true, true, rest)
    } else {
        (false, false, rest)
    };
    let (cop, message) = rest.split_once(": ")?;
    if !cop.contains('/') || cop.contains(char::is_whitespace) {
        return None;
    }
    Some(Offense {
        file: file.to_owned(),
        line: line_no,
        column,
        severity: severity.to_owned(),
        cop: cop.to_owned(),
        message: message.to_owned(),
        correctable,
        corrected,
    })
}

/// `3 files inspected, 3 offenses detected, 1 offense autocorrectable` /
/// `1 file inspected, no offenses detected` → files inspected.
pub fn parse_summary(line: &str) -> Option<u64> {
    let text = line.trim();
    let (count, rest) = text.split_once(' ')?;
    let count: u64 = count.parse().ok()?;
    (rest.starts_with("file inspected,") || rest.starts_with("files inspected,")).then_some(count)
}

/// Whether the output came from RuboCop: an offense line or the summary.
pub fn detect(output: &str) -> bool {
    output
        .lines()
        .any(|l| parse_line(l).is_some() || parse_summary(l).is_some())
}

/// Parse a full captured output.
pub fn parse(output: &str) -> Report {
    Report {
        offenses: output.lines().filter_map(parse_line).collect(),
        files_inspected: output.lines().rev().find_map(parse_summary),
    }
}

/// The text formatter's one-letter severity for a JSON `severity`.
fn severity_letter(severity: &str) -> Option<&'static str> {
    Some(match severity {
        "convention" => "C",
        "warning" => "W",
        "error" => "E",
        "fatal" => "F",
        "refactor" => "R",
        "info" => "I",
        _ => return None,
    })
}

fn is_rubocop_document(value: &serde_json::Value) -> bool {
    value.get("files").is_some_and(serde_json::Value::is_array)
        && value
            .get("summary")
            .is_some_and(serde_json::Value::is_object)
}

/// Parse the document RuboCop's JSON formatter printed in `output`, or
/// `None` when there is none (or it is incomplete). Each offense keeps the
/// location's start line and column, the cop, the message, and whether it
/// was correctable or corrected in this run.
/// <https://docs.rubocop.org/rubocop/formatters.html#json-formatter>
pub fn parse_json(output: &str) -> Option<Report> {
    let doc = super::ruby::find_json_object(output, is_rubocop_document)?;
    let mut offenses = Vec::new();
    for file in doc["files"].as_array()? {
        let Some(path) = file.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        for offense in file
            .get("offenses")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = |key: &str| offense.get(key).and_then(serde_json::Value::as_str);
            let flag = |key: &str| offense.get(key).and_then(serde_json::Value::as_bool);
            let location = &offense["location"];
            let at = |keys: [&str; 2]| {
                keys.iter()
                    .find_map(|k| location.get(*k).and_then(serde_json::Value::as_u64))
                    .and_then(|n| u32::try_from(n).ok())
            };
            let (Some(severity), Some(cop), Some(message), Some(line), Some(column)) = (
                text("severity").and_then(severity_letter),
                text("cop_name"),
                text("message"),
                at(["start_line", "line"]),
                at(["start_column", "column"]),
            ) else {
                continue;
            };
            offenses.push(Offense {
                file: path.to_owned(),
                line,
                column,
                severity: severity.to_owned(),
                cop: cop.to_owned(),
                // Some versions prefix the message with the cop name.
                message: message
                    .strip_prefix(&format!("{cop}: "))
                    .unwrap_or(message)
                    .to_owned(),
                correctable: flag("correctable").unwrap_or(false),
                corrected: flag("corrected").unwrap_or(false),
            });
        }
    }
    Some(Report {
        offenses,
        files_inspected: doc["summary"]
            .get("inspected_file_count")
            .and_then(serde_json::Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFENSES: &str = include_str!("../../tests/fixtures/rubocop-offenses.txt");
    const CLEAN: &str = include_str!("../../tests/fixtures/rubocop-clean.txt");

    #[test]
    fn detects_rubocop_by_offense_or_summary() {
        assert!(detect(OFFENSES));
        assert!(detect(CLEAN));
        assert!(detect("a.rb:1:1: C: Style/X: msg"));
        assert!(!detect("10 examples, 1 failure"));
        assert!(!detect("Inspecting 3 files\n..."));
        assert!(!detect(""));
    }

    #[test]
    fn offenses_carry_every_field() {
        let report = parse(OFFENSES);
        assert_eq!(report.files_inspected, Some(3));
        assert_eq!(report.offenses.len(), 3);
        assert_eq!(
            report.offenses[0],
            Offense {
                file: "app/models/user.rb".into(),
                line: 3,
                column: 10,
                severity: "C".into(),
                cop: "Style/StringLiterals".into(),
                message: "Prefer single-quoted strings when you don't need string interpolation or special symbols.".into(),
                correctable: false,
                corrected: false,
            }
        );
        assert_eq!(
            report.offenses[1],
            Offense {
                file: "app/models/user.rb".into(),
                line: 12,
                column: 5,
                severity: "W".into(),
                cop: "Lint/UselessAssignment".into(),
                message: "Useless assignment to variable - unused.".into(),
                correctable: true,
                corrected: false,
            }
        );
        assert_eq!(report.offenses[2].file, "app/services/order/checkout.rb");
        assert_eq!(report.offenses[2].line, 41);
        assert_eq!(report.offenses[2].column, 1);
        assert_eq!(report.offenses[2].cop, "Layout/EmptyLines");
    }

    #[test]
    fn clean_run_has_no_offenses() {
        let report = parse(CLEAN);
        assert!(report.offenses.is_empty());
        assert_eq!(report.files_inspected, Some(3));
    }

    #[test]
    fn line_grammar_edges() {
        assert!(
            parse_line("a.rb:1:1: C: [Corrected] Style/X: msg")
                .unwrap()
                .correctable
        );
        assert_eq!(
            parse_line("a.rb:1:1: E: Lint/Syntax: unexpected token")
                .unwrap()
                .severity,
            "E"
        );
        assert_eq!(parse_line("a.rb:1:1: I: Style/X: m").unwrap().severity, "I");
        // Source excerpt and caret lines are indented: never offenses.
        assert!(parse_line("  validates \"name\", presence: true").is_none());
        assert!(parse_line("            ^^^^^^").is_none());
        // Unknown severity, no cop namespace, bad numbers, no location.
        assert!(parse_line("a.rb:1:1: X: Style/X: msg").is_none());
        assert!(parse_line("a.rb:1:1: CC: Style/X: msg").is_none());
        assert!(parse_line("a.rb:1:1: C: NoSlash: msg").is_none());
        assert!(parse_line("a.rb:1:1: C: Bad Cop/X: msg").is_none());
        assert!(parse_line("a.rb:x:1: C: Style/X: msg").is_none());
        assert!(parse_line("a.rb:1:x: C: Style/X: msg").is_none());
        assert!(parse_line(":1:1: C: Style/X: msg").is_none());
        assert!(parse_line("a b.rb:1:1: C: Style/X: msg").is_none());
        assert!(parse_line("a.rb:1: C: Style/X: msg").is_none());
        assert!(parse_line("a.rb:1:1: C: Style/X").is_none());
        assert!(parse_line("a.rb:1:1: C").is_none());
        assert!(parse_line("Inspecting 3 files").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn summary_shapes() {
        assert_eq!(
            parse_summary("3 files inspected, 3 offenses detected, 1 offense autocorrectable"),
            Some(3)
        );
        assert_eq!(
            parse_summary("1 file inspected, no offenses detected"),
            Some(1)
        );
        assert_eq!(parse_summary("Inspecting 3 files"), None);
        assert_eq!(parse_summary("3 files"), None);
        assert_eq!(
            parse_summary("x files inspected, no offenses detected"),
            None
        );
        assert_eq!(parse_summary(""), None);
    }

    #[test]
    fn json_offenses_match_the_text_ones_and_keep_corrections() {
        let text = parse(OFFENSES);
        let json = parse_json(include_str!("../../tests/fixtures/rubocop-offenses.json")).unwrap();
        assert_eq!(json, text);
        let auto = parse_json(include_str!(
            "../../tests/fixtures/rubocop-autocorrect.json"
        ))
        .unwrap();
        assert_eq!(
            auto.offenses
                .iter()
                .map(|o| (o.cop.as_str(), o.corrected))
                .collect::<Vec<_>>(),
            [
                ("Style/StringLiterals", true),
                ("Lint/UselessAssignment", false)
            ]
        );
        assert!(
            parse_line("a.rb:1:1: C: [Corrected] Style/X: msg")
                .unwrap()
                .corrected
        );
        assert!(
            !parse_line("a.rb:1:1: C: [Correctable] Style/X: msg")
                .unwrap()
                .corrected
        );
        assert_eq!(parse_json("{\"examples\": [], \"summary\": {}}"), None);
        assert_eq!(parse_json("{\"files\": [{\"path\": \"a.rb\""), None);
    }
}
