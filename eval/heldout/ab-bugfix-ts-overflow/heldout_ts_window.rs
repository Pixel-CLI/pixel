// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Held-out contract for ab-bugfix-ts-overflow (copied in after the agent exits).
//! Mirrors the tests of the historical fix 313bb53 through the public API.

use pixel_session::query::parse_duration_ms;

#[test]
fn a_non_ascii_unit_is_rejected_not_a_panic() {
    assert_eq!(parse_duration_ms("é"), None);
    assert_eq!(parse_duration_ms("5é"), None);
}

#[test]
fn a_window_that_overflows_is_rejected_not_a_panic() {
    assert_eq!(parse_duration_ms("9223372036854775807d"), None);
    assert_eq!(parse_duration_ms("9223372036854775807"), None);
}

#[test]
fn the_largest_window_that_fits_still_parses() {
    assert_eq!(
        parse_duration_ms("9223372036854775s"),
        Some(9_223_372_036_854_775_000)
    );
}

#[test]
fn ordinary_windows_are_unchanged() {
    assert_eq!(parse_duration_ms("5m"), Some(300_000));
    assert_eq!(parse_duration_ms("30s"), Some(30_000));
    assert_eq!(parse_duration_ms("2h"), Some(7_200_000));
    assert_eq!(parse_duration_ms("1d"), Some(86_400_000));
    assert_eq!(parse_duration_ms("45"), Some(45_000));
    assert_eq!(parse_duration_ms("nope"), None);
    assert_eq!(parse_duration_ms(""), None);
}
