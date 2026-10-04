// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Held-out contract for ab-feature-ts-units (copied in after the agent exits).

use pixel_session::query::parse_duration_ms;

#[test]
fn weeks_are_a_unit() {
    assert_eq!(parse_duration_ms("1w"), Some(604_800_000));
    assert_eq!(parse_duration_ms("2w"), Some(1_209_600_000));
}

#[test]
fn milliseconds_are_a_unit() {
    assert_eq!(parse_duration_ms("250ms"), Some(250));
    assert_eq!(parse_duration_ms("1ms"), Some(1));
    assert_eq!(parse_duration_ms("0ms"), Some(0));
}

#[test]
fn existing_units_are_unchanged() {
    assert_eq!(parse_duration_ms("5m"), Some(300_000));
    assert_eq!(parse_duration_ms("30s"), Some(30_000));
    assert_eq!(parse_duration_ms("2h"), Some(7_200_000));
    assert_eq!(parse_duration_ms("1d"), Some(86_400_000));
    assert_eq!(parse_duration_ms("45"), Some(45_000));
}

#[test]
fn bad_windows_are_still_rejected_without_a_panic() {
    for bad in ["", "nope", "w", "ms", "é", "5é", "9223372036854775807w", "9223372036854775807d"] {
        assert_eq!(parse_duration_ms(bad), None, "{bad:?}");
    }
}
