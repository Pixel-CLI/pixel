// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for `pixel-facts`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod excavate_rescue_v2;
mod facts_integration;
