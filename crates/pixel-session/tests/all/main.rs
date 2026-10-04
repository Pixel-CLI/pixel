// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for `pixel-session`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod query;
mod run_wrapper;
mod store;
