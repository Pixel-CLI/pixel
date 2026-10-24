// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The `pixel` binary: deterministic evidence for coding agents
//! (`specs/thesis.md`). `run` owns an invocation from argv to exit status.

mod classify;
mod classify_config;
mod classify_setup;
mod cli;
mod client;
mod commands;
mod config_cmd;
mod config_file;
mod decide_clef;
mod decide_infisical;
mod decide_jev;
mod decide_ollaya;
mod decide_remote;
mod doctor_cmd;
mod hook;
mod index_cmd;
mod meaning_cmd;
mod metrics_cmd;
mod plugin_cmd;
mod render;
mod run;
mod select;
mod serve_trace;
mod settings;
mod setup_cmd;
mod task_intent;
mod workspace_cmd;

pub use pixel_metrics::operation;

/// One lock for every unit test that mutates process-wide state (`HOME` and
/// friends): two module-local locks cannot see each other, so all such tests
/// share this single mutex.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn main() -> std::process::ExitCode {
    run::run()
}
