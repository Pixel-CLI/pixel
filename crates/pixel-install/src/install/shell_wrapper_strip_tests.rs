// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::{PIXEL_MANAGED_BEGIN, PIXEL_MANAGED_END, strip_shell_wrappers};
use crate::InstallError;

#[test]
fn a_terminated_block_is_stripped_and_the_surrounding_lines_kept() {
    let profile = format!(
        "alias first='one'\n{PIXEL_MANAGED_BEGIN}\nclaude() {{ :; }}\n{PIXEL_MANAGED_END}\nalias last='two'\n"
    );
    let cleaned = strip_shell_wrappers(&profile).expect("a closed block is strippable");
    assert_eq!(cleaned, "alias first='one'\nalias last='two'\n");
    assert!(
        !cleaned.contains(PIXEL_MANAGED_BEGIN),
        "the block must be gone"
    );
}

#[test]
fn a_begin_without_an_end_is_refused_not_stripped_to_eof() {
    let broken = format!("{PIXEL_MANAGED_BEGIN}\nalias keep='me'\n");
    assert!(
        matches!(
            strip_shell_wrappers(&broken),
            Err(InstallError::UnterminatedManagedBlock)
        ),
        "an unterminated block must be refused: the lines after it are the user's"
    );
}

#[test]
fn a_stray_end_marker_without_a_begin_is_kept_as_user_content() {
    let profile = format!("alias mine='kept'\n{PIXEL_MANAGED_END}\n");
    assert_eq!(strip_shell_wrappers(&profile).unwrap(), profile);
}
