// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Parser for `tsc --pretty false` diagnostics:
//! `path(line,col): error TS1234: message`

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TscError {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub code: String,
    pub message: String,
}

/// Parse one `--pretty false` diagnostic line; `None` for anything else.
pub fn parse_line(line: &str) -> Option<TscError> {
    // Anchor on "): error TS" — the stable middle of the format.
    let marker = "): error TS";
    let marker_at = line.find(marker)?;
    let (left, rest) = line.split_at(marker_at);
    // rest = "): error TS1234: message"
    let rest = &rest[2..]; // strip "):"
    let rest = rest.strip_prefix(" error ")?;
    let colon = rest.find(": ")?;
    let code = &rest[..colon];
    if !code.starts_with("TS") || !code[2..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let message = rest[colon + 2..].to_owned();
    // left = "path(line,col"
    let paren = left.rfind('(')?;
    let file = left[..paren].to_owned();
    let (line_str, col_str) = left[paren + 1..].split_once(',')?;
    Some(TscError {
        file,
        line: line_str.trim().parse().ok()?,
        column: col_str.trim().parse().ok()?,
        code: code.to_owned(),
        message,
    })
}

/// Parse a full captured output; non-diagnostic lines are ignored.
pub fn parse(output: &str) -> Vec<TscError> {
    output.lines().filter_map(parse_line).collect()
}

/// Group diagnostics by TS code, preserving first-seen order of insertion
/// within a deterministic (sorted) code order.
pub fn group_by_code(errors: &[TscError]) -> BTreeMap<String, Vec<&TscError>> {
    let mut groups: BTreeMap<String, Vec<&TscError>> = BTreeMap::new();
    for error in errors {
        groups.entry(error.code.clone()).or_default().push(error);
    }
    groups
}

/// `"23 errors in 7 files"` — the human summary for the summary record.
pub fn summary_message(errors: &[TscError]) -> String {
    let files: std::collections::BTreeSet<&str> = errors.iter().map(|e| e.file.as_str()).collect();
    let error_word = if errors.len() == 1 { "error" } else { "errors" };
    let file_word = if files.len() == 1 { "file" } else { "files" };
    format!(
        "{} {error_word} in {} {file_word}",
        errors.len(),
        files.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
src/routes/chat.tsx(88,14): error TS2339: Property 'x' does not exist on type 'Sessions'.
src/routes/chat.tsx(92,3): error TS2339: Property 'y' does not exist on type 'Sessions'.
src/lib/api.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
some random noise line
error TS9999 without location is not a diagnostic
";

    #[test]
    fn parses_diagnostic_lines_only() {
        let errors = parse(FIXTURE);
        assert_eq!(errors.len(), 3);
        assert_eq!(
            errors[0],
            TscError {
                file: "src/routes/chat.tsx".into(),
                line: 88,
                column: 14,
                code: "TS2339".into(),
                message: "Property 'x' does not exist on type 'Sessions'.".into(),
            }
        );
        assert_eq!(errors[2].code, "TS2322");
    }

    #[test]
    fn handles_parens_in_path() {
        let error = parse_line("src/app (copy)/x.ts(1,2): error TS1005: ';' expected.").unwrap();
        assert_eq!(error.file, "src/app (copy)/x.ts");
        assert_eq!(error.line, 1);
        assert_eq!(error.code, "TS1005");
    }

    #[test]
    fn rejects_non_diagnostics() {
        assert!(parse_line("Found 3 errors.").is_none());
        assert!(parse_line("x.ts(1,2): warning TS1: nope").is_none());
        assert!(parse_line("x.ts(a,b): error TS1: bad numbers").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn groups_and_summarizes() {
        let errors = parse(FIXTURE);
        let groups = group_by_code(&errors);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["TS2339"].len(), 2);
        assert_eq!(groups["TS2322"].len(), 1);
        assert_eq!(summary_message(&errors), "3 errors in 2 files");
        assert_eq!(summary_message(&errors[..1]), "1 error in 1 file");
    }
}
