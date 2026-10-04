// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::should_offer_classify_setup;

#[test]
fn prompt_runs_only_for_an_interactive_non_json_global_install() {
    assert!(should_offer_classify_setup(true, false, true, true));
    assert!(!should_offer_classify_setup(false, false, true, true));
    assert!(!should_offer_classify_setup(true, true, true, true));
    assert!(!should_offer_classify_setup(true, false, false, true));
    assert!(!should_offer_classify_setup(true, false, true, false));
}
