// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shared build and invocation provenance joins local journals without raw host identifiers.

use std::sync::OnceLock;

use pixel_actionlog::{ActionEvent, Provenance};

const SCHEMA_VERSION: u32 = 1;
static INVOCATION: OnceLock<String> = OnceLock::new();
static HOST: OnceLock<(String, Option<String>)> = OnceLock::new();

/// Bind the native hook payload before its worker threads start.
pub(crate) fn bind_hook(provider: &str, session: Option<&str>) {
    let _ = HOST.set((
        provider.to_owned(),
        session.map(crate::execution_brief::feedback::session_key),
    ));
}

/// Record only explicit host context; an ordinary shell has no inferred provider.
pub(crate) fn provenance(source: &str) -> Provenance {
    let host = HOST.get().cloned().or_else(|| {
        std::env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .map(|id| {
                (
                    "codex".to_owned(),
                    Some(crate::execution_brief::feedback::session_key(&id)),
                )
            })
    });
    Provenance {
        schema_version: SCHEMA_VERSION,
        pixel_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_commit: env!("PIXEL_BUILD_COMMIT").to_owned(),
        build_dirty: env!("PIXEL_GIT_DIRTY").parse().ok(),
        source: source.to_owned(),
        invocation_id: INVOCATION
            .get_or_init(pixel_actionlog::invocation_id)
            .clone(),
        provider: host.as_ref().map(|(provider, _)| provider.clone()),
        session: host.and_then(|(_, session)| session),
    }
}

/// Construct a CLI/plugin action with the same identity as its brief records.
pub(crate) fn action(command: &str, args: String, source: &str) -> ActionEvent {
    let provenance = provenance(source);
    let mut event = ActionEvent::new(command, args);
    event.invocation_id = Some(provenance.invocation_id.clone());
    event.provenance = Some(provenance);
    event
}
