// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

fn numbers(text: &str) -> Vec<&str> {
    version_spans(text)
        .into_iter()
        .map(|(_, _, number)| number)
        .collect()
}

#[test]
fn a_version_is_dotted_digits_not_glued_to_a_word_or_a_dot() {
    assert_eq!(numbers("v5.3.0 and 3.10"), ["5.3.0", "3.10"]);
    assert_eq!(numbers("see 2.0. Then -1.5, (4.2)"), ["2.0", "1.5", "4.2"]);
    assert_eq!(numbers("1.2.x"), ["1.2"]);
    assert_eq!(numbers("v1.2"), ["1.2"]);
    for glued in ["x3.10", "1.2.3a", "1.2_3", "a.1.2", "3", "v3", "3.", "10.a"] {
        assert_eq!(numbers(glued), Vec::<&str>::new(), "{glued}");
    }
}

#[test]
fn the_spans_cover_the_optional_v_and_replace_by_a_space() {
    assert_eq!(version_spans("in v5.3.0 now"), [(3, 9, "5.3.0")]);
    assert_eq!(without_versions("in v5.3.0 now, 3.10"), "in   now,  ");
    assert_eq!(without_versions("no version"), "no version");
}

#[test]
fn versions_are_listed_once_in_the_order_quoted() {
    assert_eq!(versions("3.10 then v2.0 then 3.10"), ["3.10", "2.0"]);
}

#[test]
fn stems_are_four_letters_of_the_words_a_history_question_does_not_use() {
    let held = stems("Which commits changed the Redirect handling? Parsers");
    let mut held: Vec<_> = held.into_iter().collect();
    held.sort();
    assert_eq!(held, ["hand", "pars", "redi"]);
}

#[test]
fn subject_words_keep_letters_and_digits_and_drop_what_says_nothing() {
    assert_eq!(
        subject_words("Which commit added the v2 CLI parser to a build?"),
        ["v2", "cli", "parser", "build"]
    );
    assert_eq!(subject_words("x I"), Vec::<String>::new());
}

#[test]
fn quoted_titles_are_between_quotes_of_eight_to_one_hundred_twenty_characters() {
    assert_eq!(
        quoted("what did \"fix the archive reader\" and `bump 1.2.3 now` do, or 'short'"),
        ["fix the archive reader", "bump 1.2.3 now"]
    );
    let long = "x".repeat(121);
    assert_eq!(quoted(&format!("\"{long}\"")), Vec::<String>::new());
    assert_eq!(quoted("\"  padded title  \""), ["padded title"]);
}

#[test]
fn a_line_has_its_id_first_and_its_subject_after_the_date() {
    assert_eq!(id_of("abc1234 2026-01-02 fix: it"), Some("abc1234"));
    assert_eq!(id_of("  "), None);
    assert_eq!(subject_of("abc1234 2026-01-02 fix: it now"), "fix: it now");
    assert_eq!(subject_of("abc1234 2026-01-02"), "2026-01-02");
    assert_eq!(subject_of(""), "");
}

#[test]
fn a_grep_argument_is_made_for_each_of_the_first_words() {
    let words: Vec<String> = ["alpha", "beta", "gamma"].map(ToOwned::to_owned).to_vec();
    assert_eq!(greps(&words, 2), ["--grep=alpha", "--grep=beta"]);
    assert_eq!(greps(&words, 5).len(), 3);
}
